//! The code hand (TOOL-04): run one JavaScript script per invocation in
//! a fresh embedded QuickJS runtime (rquickjs, behind the default-OFF
//! `code` cargo feature). The script orchestrates ka tools through two
//! bindings and ONLY its final value returns to the model, so multi-step
//! tool chains cost one round-trip instead of one per step.
//!
//! Isolation is total by construction: the runtime, its heap and its
//! globals are created fresh for every call and dropped with it —
//! nothing persists between invocations, so a global written in one run
//! is gone in the next. No host modules are bound (no fs, net, process,
//! stdio): the script sees exactly the `ka` object below, nothing else
//! (fail-closed).
//!
//! Bindings (async, driven by the runtime's job queue):
//! - `ka.tools.call(name, argsJson) -> Promise<string>` — routes through
//!   the session's hand registry with the same clearance gate a model
//!   call gets (features toggles, rules, mode, hardstops, protected
//!   paths). A gate verdict the model could answer with an ask is
//!   surfaced to the user as a note and DENIED to the script (asks
//!   cannot be answered from a running script) as a
//!   `{"error":"denied: …"}` string; the binding never throws. Tool
//!   execution errors resolve to `{"error": …}` strings too, so the
//!   script can branch on JSON.
//! - `ka.models.classify(text, labels) -> Promise<{label, confidence}>`
//!   — one capped completion through the session's fast role (the
//!   auto-review model). Without a fast role configured the promise
//!   REJECTS with an instructive message.
//!
//! Two limits bound every run, both config-authored (`[code]`): the
//! wall-clock budget aborts execution through the QuickJS interrupt
//! handler (plus a tokio-side belt for hung tool bindings), and the
//! QuickJS heap cap aborts runaway allocation. Errors come back as
//! one actionable `ToolOutput::err`, with the script's own line number
//! when the runtime provides one.
//!
//! Gate scope note: the routed gate mirrors the voice's admit gate for
//! everything visible outside it (toggles, `[[rules]]`,
//! `[permissions] allow`, mode, hardstops, protected paths, prompt
//! policy). Session-remembered "always" answers, per-turn tool
//! allowlists, hooks and the loop guard live inside the turn machinery
//! a hand cannot reach and are intentionally not re-implemented here.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ka_protocol::{Event, Mode};
use rquickjs::function::Async;
use rquickjs::{AsyncContext, AsyncRuntime, Ctx, Function, Promise};
use serde_json::{Value, json};

use super::{Clearance, Hand, HandContext, HandDef, ToolOutput};
use crate::config::{Code, PromptPolicy, Rule, Verdict, evaluate_rules};
use crate::voice::AuxRole;

/// Final-value cap returned to the model: a larger script result parks
/// whole in a spill file and the excerpt points at it (the standard
/// caps/spill hygiene pipe).
const RESULT_CAP: usize = 8 * 1024;

/// One capped completion deadline for `ka.models.classify` — the same
/// budget the auto-review and injection-check fast-role calls use.
const CLASSIFY_TIMEOUT: Duration = Duration::from_secs(10);

/// Grace added to the `[code]` timeout on the tokio side. The interrupt
/// handler cuts the JS itself at the deadline; this belt catches a run
/// wedged inside a tool binding that never yields back to the engine.
const RUN_GRACE: Duration = Duration::from_millis(100);

/// The fast-role classifier's instructions: one label back, nothing else.
const CLASSIFY_SYSTEM: &str = "You classify text for a coding agent. Reply with \
exactly one label from the provided list and nothing else.";

/// Everything a routed script call needs beyond the per-run
/// [`HandContext`]: the hands scripts may call and the permission
/// posture the gate mirrors. Built once at registration; every
/// invocation shares it (it holds no per-script state).
pub struct CodeRouter {
    /// Hands visible to scripts: the built-in session registry at
    /// registration time (read, edit, write, bash, glob, grep, todo,
    /// jobs, cron, monitor, …). Hands registered after it — MCP tools,
    /// delegate, lsp_*, web — are model-facing only and resolve to
    /// `unknown tool` here.
    hands: Vec<Arc<dyn Hand>>,
    /// `[[rules]]`, first-match-wins (same evaluation the gate uses).
    rules: Vec<Rule>,
    /// Session permission mode.
    mode: Mode,
    /// `[permissions] prompt_policy` for reviewable asks.
    prompt_policy: PromptPolicy,
    /// `[permissions] allow` persistent allowlist.
    allowed_tools: Vec<String>,
    /// Session feature toggles (hide/reject, same as the admit path).
    features: crate::features::FeatureSlot,
    /// Session events: a gate ask the script cannot answer surfaces
    /// here as a note so the user still sees what was requested.
    events: tokio::sync::mpsc::Sender<Event>,
    /// The fast-role completion handle (`[roles] fast`); `None` makes
    /// `ka.models.classify` reject until the user configures one.
    fast_role: Option<AuxRole>,
}

/// One gate verdict for a routed call.
enum Decision {
    Allow,
    /// Refused outright — the reason is fed to the script verbatim.
    Deny(String),
    /// The model-facing gate would ask the user; from a script this
    /// surfaces as a note and resolves to a denial string.
    Ask {
        question: String,
    },
}

impl CodeRouter {
    /// Assemble the router from the session posture (engine bootstrap).
    /// Crate-visible: the only caller is the engine's registration
    /// block, and the fast-role handle is a crate type.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        hands: Vec<Arc<dyn Hand>>,
        rules: Vec<Rule>,
        mode: Mode,
        prompt_policy: PromptPolicy,
        allowed_tools: Vec<String>,
        features: crate::features::FeatureSlot,
        events: tokio::sync::mpsc::Sender<Event>,
        fast_role: Option<AuxRole>,
    ) -> Self {
        Self {
            hands,
            rules,
            mode,
            prompt_policy,
            allowed_tools,
            features,
            events,
            fast_role,
        }
    }

    /// `ka.tools.call`: run one routed call and render its outcome as
    /// the string the script resolves to. Success → the tool's content;
    /// every refusal/error → a `{"error": …}` JSON string (the binding
    /// never throws; scripts branch on the JSON instead).
    pub async fn tools_call(&self, name: &str, args_json: &str, hctx: &HandContext) -> String {
        let args: Value = match serde_json::from_str(args_json) {
            Ok(v @ Value::Object(_)) => v,
            Ok(_) => return error_json("tool arguments must be a JSON object"),
            Err(e) => return error_json(&format!("invalid tool arguments JSON: {e}")),
        };
        let out = self.route(name, &args, hctx).await;
        if out.is_error {
            error_json(&out.content)
        } else {
            out.content
        }
    }

    /// One routed call through the SAME registry + gate shape a model
    /// tool call takes: toggles → allowlist → hand lookup → clearance
    /// gate → hand execution.
    async fn route(&self, name: &str, args: &Value, hctx: &HandContext) -> ToolOutput {
        // session feature toggles: identical guard + wording to the
        // voice's admit-time rejection
        if let Some(spec) = self.features.read().hidden_reason(name) {
            return ToolOutput::err(format!(
                "{name} is disabled by a session feature toggle (`{spec}`); \
                 it is unavailable until the user re-enables it"
            ));
        }
        if !self.features.read().tool_allowed(name) {
            let allowed = self.features.read().allow_entries().join(", ");
            return ToolOutput::err(format!(
                "{name} is not in this session's [features] allow list ({allowed}); \
                 it is unavailable"
            ));
        }
        let Some(hand) = self.hands.iter().find(|h| h.def().name == name).cloned() else {
            return ToolOutput::err(format!(
                "unknown tool {name} — scripts can call this session's built-in \
                 hands by name (read, grep, glob, bash, edit, …)"
            ));
        };
        match self.gate(&*hand, name, args, hctx) {
            Decision::Allow => hand.execute(args, hctx).await,
            Decision::Deny(reason) => ToolOutput::err(format!("denied: {reason}")),
            Decision::Ask { question } => {
                // an ask is unanswerable mid-script: the user still sees
                // what was requested (as a note), the script gets a
                // denial that says how to unblock it
                self.events
                    .send(Event::Note {
                        message: format!(
                            "code script asked: {question} — it cannot be answered \
                             from a running script; pre-approve it (rule or \
                             [permissions] allow) and re-run"
                        ),
                    })
                    .await
                    .ok();
                ToolOutput::err(format!(
                    "denied: {question} — permission asks cannot be answered from \
                     a running script; pre-approve it (rule or [permissions] allow) \
                     and re-run"
                ))
            }
        }
    }

    /// The scoped clearance gate, mirroring the voice's `gate()`:
    /// protected paths (hardstop class) first, then `[[rules]]`
    /// first-match-wins, the persistent allowlist, and the
    /// per-clearance mode logic with hardstop analysis for exec.
    fn gate(&self, hand: &dyn Hand, name: &str, args: &Value, hctx: &HandContext) -> Decision {
        let call = ka_dialect::speaker::ToolCall {
            id: "code-script".to_string(),
            tool: name.to_string(),
            arguments: args.clone(),
        };
        let primary = call.primary_arg();
        let clearance = hand.clearance_for(args);
        // protected paths outrank every posture below (except readonly
        // mode, which already denies the same set via the plans-dir rule)
        if clearance == Clearance::Write && self.mode != Mode::Readonly {
            if let Some(reason) = super::protected::reason(&hctx.cwd, &primary) {
                return self.ask(
                    format!("PROTECTED — {reason}: `{primary}`. Proceed anyway?"),
                    false,
                );
            }
        }
        // configured rules: first match wins, before mode logic
        if let Some(verdict) = evaluate_rules(&self.rules, name, &primary) {
            return match verdict {
                Verdict::Allow => Decision::Allow,
                Verdict::Ask => self.ask(
                    format!("rule requires confirmation for {name} `{primary}`"),
                    false,
                ),
                Verdict::Deny => Decision::Deny(format!("rule for {name}")),
            };
        }
        if self.allowed_tools.iter().any(|t| t == name) {
            return Decision::Allow;
        }
        match clearance {
            Clearance::Read => Decision::Allow,
            Clearance::Write => match self.mode {
                Mode::Allow | Mode::AcceptEdits => Decision::Allow,
                Mode::Ask | Mode::DontAsk => {
                    self.ask(format!("allow {name} to modify files?"), true)
                }
                Mode::Readonly => {
                    // research mode: only the project plans dir is writable
                    let plans_dir = crate::project_root(&hctx.cwd).join(".ka/plans");
                    let path = super::read::resolve(hctx, &primary);
                    if path.starts_with(&plans_dir) {
                        Decision::Allow
                    } else {
                        Decision::Deny(format!(
                            "plan mode is read-only except .ka/plans/ (got {primary:?}); \
                             use /build to switch to implementation"
                        ))
                    }
                }
            },
            Clearance::Exec => {
                let command = args
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let analysis = super::bashp::analyze(command);
                if let Some(stop) = super::bashp::hardstop(command, &analysis) {
                    // hardstops keep asking in every posture — and a
                    // script can never answer one
                    return self.ask(
                        format!("HARDSTOP — {}: `{command}`. Proceed anyway?", stop.reason),
                        false,
                    );
                }
                if super::bashp::all_readonly(&analysis) {
                    return Decision::Allow;
                }
                match self.mode {
                    Mode::Allow => Decision::Allow,
                    Mode::Readonly => self.ask(
                        format!("plan mode: run `{command}`? (build with /build)"),
                        false,
                    ),
                    Mode::AcceptEdits | Mode::Ask | Mode::DontAsk => {
                        self.ask(format!("run `{command}`?"), true)
                    }
                }
            }
        }
    }

    /// The ask-class verdict with the session posture applied: a deny
    /// prompt policy or dont-ask mode converts the question into a
    /// denial (same wording as the voice's `ask_gate`); anything else
    /// stays an ask the script cannot answer.
    fn ask(&self, question: String, reviewable: bool) -> Decision {
        if reviewable && self.prompt_policy == PromptPolicy::Deny {
            return Decision::Deny(format!("prompt policy denies: {question}"));
        }
        if self.mode == Mode::DontAsk {
            return Decision::Deny(format!("dontAsk mode denies: {question}"));
        }
        Decision::Ask { question }
    }

    /// `ka.models.classify`: one capped fast-role completion mapping
    /// `text` onto one of `labels`. Exact (case-insensitive) matches
    /// carry confidence 1.0; a reply that merely names a label carries
    /// 0.6; anything else is an error naming the reply.
    async fn classify(&self, text: &str, labels: &[String]) -> Result<(String, f64), String> {
        let Some(aux) = &self.fast_role else {
            return Err(
                "ka.models.classify: no fast role configured — set [roles] fast \
                 in ka.toml to enable it"
                    .to_string(),
            );
        };
        if labels.is_empty() {
            return Err("ka.models.classify: labels must not be empty".to_string());
        }
        let list = labels.join(", ");
        let prompt = format!(
            "Classify the text into exactly one of these labels: {list}.\n\n\
             Text:\n{text}\n\nReply with exactly one label from the list and \
             nothing else."
        );
        let Some(reply) = aux
            .complete(CLASSIFY_SYSTEM, &prompt, CLASSIFY_TIMEOUT)
            .await
        else {
            return Err(
                "ka.models.classify: the fast-role classifier failed or timed out".to_string(),
            );
        };
        let reply = reply.trim();
        if let Some(label) = labels.iter().find(|l| l.trim().eq_ignore_ascii_case(reply)) {
            return Ok((label.clone(), 1.0));
        }
        let lowered = reply.to_lowercase();
        if let Some(label) = labels
            .iter()
            .find(|l| lowered.contains(&l.trim().to_lowercase()))
        {
            return Ok((label.clone(), 0.6));
        }
        Err(format!(
            "ka.models.classify: the classifier reply {reply:?} does not name \
             one of the labels ({list})"
        ))
    }
}

/// `{"error": …}` rendering for script-facing tool outcomes.
fn error_json(message: &str) -> String {
    json!({ "error": message }).to_string()
}

/// The code tool: one `{ script }` string in, one final value out.
pub struct CodeHand {
    /// Shared routing + gate posture.
    router: Arc<CodeRouter>,
    /// `[code]` limits (timeout + memory), resolved config values.
    limits: Code,
}

impl CodeHand {
    /// Hand over the router and the resolved `[code]` limits.
    pub fn new(router: Arc<CodeRouter>, limits: Code) -> Self {
        Self { router, limits }
    }

    /// Run the script in a fresh runtime; only the final value (or one
    /// actionable error) survives.
    async fn run_script(&self, script: &str, hctx: &HandContext) -> ToolOutput {
        let timeout_ms = self.limits.effective_timeout_ms();
        let memory_mb = self.limits.effective_memory_mb();
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);

        let rt = match AsyncRuntime::new() {
            Ok(rt) => rt,
            Err(e) => return ToolOutput::err(format!("code: QuickJS runtime init failed: {e}")),
        };
        rt.set_memory_limit(memory_mb as usize * 1024 * 1024).await;
        // checked while JS executes; past the deadline it raises an
        // uncatchable exception, so even `while (true) {}` ends on time.
        // Owned by the runtime and dropped with it.
        rt.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)))
            .await;

        let ctx = match AsyncContext::full(&rt).await {
            Ok(ctx) => ctx,
            Err(e) => return ToolOutput::err(format!("code: QuickJS context init failed: {e}")),
        };

        // a NEW runtime per invocation: no globals, no heap, no bindings
        // survive this call
        let router = self.router.clone();
        let hctx = hctx.clone();
        let park_hctx = hctx.clone();
        let run = ctx.async_with(async move |ctx| {
            run_locked(ctx, router, hctx, script, deadline, timeout_ms, memory_mb).await
        });
        let run = tokio::time::timeout(Duration::from_millis(timeout_ms) + RUN_GRACE, run);
        let text = match run.await {
            Err(_) => {
                return ToolOutput::err(format!(
                    "code: script timed out after {timeout_ms} ms ([code] timeout_ms)"
                ));
            }
            Ok(Err(message)) => return ToolOutput::err(message),
            Ok(Ok(text)) => text,
        };
        // leftover microtask jobs hold GC references (the async wrapper's
        // own completion jobs included); draining them before the drop is
        // what lets QuickJS free the runtime without tripping its
        // gc_obj_list assertion
        while rt.execute_pending_job().await.unwrap_or(false) {}
        drop(ctx);
        if text.len() <= RESULT_CAP {
            return ToolOutput::ok(text);
        }
        let kb = text.len().div_ceil(1024);
        match park_hctx.spill.park(&text) {
            Ok(pointer) => {
                let mut excerpt: String = text.chars().take(RESULT_CAP).collect();
                excerpt.push_str(&format!(
                    "\n[script result is {kb} KB — full value parked at {pointer}]"
                ));
                let mut out = ToolOutput::ok(excerpt);
                out.spill = Some(pointer);
                out
            }
            Err(_) => ToolOutput::err(format!(
                "code: script produced a {kb} KB result (over the result cap) \
                 and it could not be parked in the spill store"
            )),
        }
    }
}

impl Hand for CodeHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "code".to_string(),
            description: "Run a short JavaScript script that orchestrates ka tools and \
                return ONLY its final value (fewer round-trips than one tool call per \
                step). Arguments: {\"script\": \"…\"}. Inside the script: \
                `ka.tools.call(name, argsJson)` awaits one ka tool call (same tools \
                and permission gate as a direct call; gate refusals resolve to a \
                `{\"error\": \"…\"}` string — JSON.parse it to branch) and \
                `ka.models.classify(text, labels)` awaits a fast-role classification \
                `{label, confidence}` (rejects without [roles] fast). No fs/network \
                access; every run starts fresh (no state between runs). The script's \
                final `return` value is JSON-serialized back to you; keep scripts \
                bounded — they abort at the configured timeout and memory cap, and \
                results over 8 KiB are parked in a spill file."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "script": {
                        "type": "string",
                        "description": "JavaScript source; top-level await and return \
                            are allowed (run inside an async wrapper)"
                    }
                },
                "required": ["script"]
            }),
            clearance: Clearance::Exec,
            read_only: false,
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        ctx: &'a HandContext,
    ) -> std::pin::Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            let Some(script) = args.get("script").and_then(Value::as_str) else {
                return ToolOutput::err("code: missing required 'script'");
            };
            self.run_script(script, ctx).await
        })
    }
}

/// One locked runtime pass: install the `ka` bindings, eval the script
/// wrapped in an async IIFE, await its promise, serialize the value.
/// Every failure path returns the actionable message for the tool
/// result (runtime line numbers shift down one — the wrapper owns
/// source line 1).
async fn run_locked<'js>(
    ctx: Ctx<'js>,
    router: Arc<CodeRouter>,
    hctx: HandContext,
    script: &str,
    deadline: Instant,
    timeout_ms: u64,
    memory_mb: u32,
) -> Result<String, String> {
    if let Err(e) = install_bindings(&ctx, &router, &hctx) {
        return Err(format!("code: binding setup failed: {e}"));
    }
    let source = wrap_script(script);
    let describe = |e: rquickjs::Error| describe_failure(&ctx, e, deadline, timeout_ms, memory_mb);
    let promise: Promise = match ctx.eval(source.as_bytes()) {
        Ok(p) => p,
        Err(e) => return Err(describe(e)),
    };
    let value: rquickjs::Value = match promise.into_future().await {
        Ok(v) => v,
        Err(e) => return Err(describe(e)),
    };
    match ctx.json_stringify(value) {
        Ok(Some(serialized)) => serialized
            .to_string()
            .map_err(|e| format!("code: could not stringify the script result: {e}")),
        // a bare `return` (undefined) serializes to nothing
        Ok(None) => Ok("undefined".to_string()),
        Err(e) => Err(format!(
            "code: script result is not JSON-serializable ({e}); return plain \
             JSON values (no circular structures or BigInt)"
        )),
    }
}

/// Install the script's bindings. The Rust side is two plain async
/// functions whose payloads capture NO `Ctx`: a context reference
/// inside a function payload outlives the free order QuickJS expects
/// (context freed, then runtime) and trips its gc_obj_list free
/// assertion. The `ka` object the script actually sees is a tiny JS
/// shim over those two functions, evaluated once per run — it adds
/// nothing else to the sandbox (fail-closed: the two `__ka_` functions
/// behave exactly like their `ka.*` wrappers).
fn install_bindings(
    ctx: &Ctx<'_>,
    router: &Arc<CodeRouter>,
    hctx: &HandContext,
) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let call_router = router.clone();
    let call_hctx = hctx.clone();
    globals.set(
        "__ka_tools_call",
        Function::new(
            ctx.clone(),
            Async(move |name: String, args_json: String| {
                let router = call_router.clone();
                let hctx = call_hctx.clone();
                async move { router.tools_call(&name, &args_json, &hctx).await }
            }),
        )?,
    )?;
    let classify_router = router.clone();
    globals.set(
        "__ka_classify",
        Function::new(
            ctx.clone(),
            Async(move |text: String, labels: Vec<String>| {
                let router = classify_router.clone();
                async move { classify_json(router, text, labels).await }
            }),
        )?,
    )?;
    ctx.eval::<(), _>(
        br#"
globalThis.ka = {
    tools: { call: (name, argsJson) => __ka_tools_call(name, argsJson) },
    models: {
        classify: (text, labels) =>
            __ka_classify(text, labels).then((json) => JSON.parse(json)),
    },
};
"#,
    )?;
    Ok(())
}

/// The classify binding's future: resolves a `{"label": …,
/// "confidence": …}` JSON string the shim parses into the object, or
/// rejects with the router's actionable message as a JS Error (the
/// `FromJs` error variant throws as a TypeError carrying it).
async fn classify_json(
    router: Arc<CodeRouter>,
    text: String,
    labels: Vec<String>,
) -> std::result::Result<String, rquickjs::Error> {
    match router.classify(&text, &labels).await {
        Ok((label, confidence)) => Ok(json!({
            "label": label,
            "confidence": confidence,
        })
        .to_string()),
        Err(message) => Err(rquickjs::Error::FromJs {
            from: "ka.models.classify",
            to: "classify",
            message: Some(message),
        }),
    }
}

/// Wrap the user script so `return` and top-level `await` work: line 1
/// is the wrapper, so reported line numbers shift down by one
/// ([`shift_line`]).
fn wrap_script(script: &str) -> String {
    format!("(async () => {{\n{script}\n}})()")
}

/// Render one runtime failure as the tool result message: timeouts
/// first (the interrupt handler surfaces as a plain exception), then
/// the memory cap, then the exception itself with its script-side line
/// when the runtime provides a stack.
fn describe_failure(
    ctx: &Ctx<'_>,
    e: rquickjs::Error,
    deadline: Instant,
    timeout_ms: u64,
    memory_mb: u32,
) -> String {
    if Instant::now() >= deadline {
        return format!("code: script timed out after {timeout_ms} ms ([code] timeout_ms)");
    }
    match e {
        rquickjs::Error::Allocation => out_of_memory(memory_mb),
        rquickjs::Error::Exception => describe_exception(ctx, memory_mb),
        other => format!("code: script failed: {other}"),
    }
}

/// The uncaught-exception rendering (syntax errors land here too, as
/// SyntaxError exceptions with a stack).
fn describe_exception(ctx: &Ctx<'_>, memory_mb: u32) -> String {
    let caught = ctx.catch();
    if caught.is_undefined() {
        return "code: uncaught exception".to_string();
    }
    let Some(exc) = caught.clone().into_exception() else {
        return match ctx.json_stringify(caught) {
            Ok(Some(rendered)) => match rendered.to_string() {
                Ok(rendered) => format!("code: uncaught exception: {rendered}"),
                Err(_) => "code: uncaught exception".to_string(),
            },
            _ => "code: uncaught exception".to_string(),
        };
    };
    let message = exc
        .message()
        .unwrap_or_else(|| "uncaught exception".to_string());
    if message.contains("out of memory") {
        return out_of_memory(memory_mb);
    }
    let mut out = format!("code: uncaught exception: {message}");
    if let Some(stack) = exc.stack() {
        if let Some(frame) = stack.lines().next() {
            let frame = frame.to_string();
            out.push_str(&format!(" (at {})", shift_line(&frame)));
        }
    }
    out
}

fn out_of_memory(memory_mb: u32) -> String {
    format!(
        "code: script aborted: exceeded its memory limit ({memory_mb} MB, \
         [code] memory_mb)"
    )
}

/// Shift a stack frame's line number down one: the wrapper owns source
/// line 1, so runtime `file:line:col` frames point one past the
/// script's own line. Handles the parenthesised form (`at fn
/// (file:4:1)`) and the bare form (`at file:4:1`); best-effort —
/// frames that do not parse keep their raw text.
fn shift_line(frame: &str) -> String {
    let trimmed = frame.trim();
    let body = trimmed.strip_prefix("at ").unwrap_or(trimmed);
    let (head, loc) = match body.rsplit_once('(') {
        Some((head, inner)) => (Some(head.trim()), inner.trim_end_matches(')')),
        None => (None, body),
    };
    // loc: file:line:col, or file:line
    let (file, line_str, col) = match loc.rsplit_once(':') {
        Some((before, col)) => match before.rsplit_once(':') {
            Some((file, line)) => (file, line, Some(col)),
            None => (before, col, None),
        },
        None => return frame.to_string(),
    };
    let Ok(line) = line_str.parse::<usize>() else {
        return frame.to_string();
    };
    let mut out = String::with_capacity(frame.len() + 1);
    out.push_str("at ");
    if let Some(head) = head {
        out.push_str(head);
        out.push(' ');
    }
    out.push('(');
    out.push_str(file);
    out.push(':');
    out.push_str(&line.saturating_sub(1).max(1).to_string());
    if let Some(col) = col {
        out.push(':');
        out.push_str(col);
    }
    out.push(')');
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use ka_dialect::speaker::{SpeakFuture, SpeakRequest, StreamEvent};
    use ka_protocol::{Stop, Usage};

    struct EchoHand;
    impl Hand for EchoHand {
        fn def(&self) -> HandDef {
            HandDef {
                name: "echo".to_string(),
                description: "test fixture: echoes its text".to_string(),
                parameters: json!({"type": "object"}),
                clearance: Clearance::Read,
                read_only: true,
            }
        }

        fn execute<'a>(
            &'a self,
            args: &'a Value,
            _ctx: &'a HandContext,
        ) -> std::pin::Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
            Box::pin(async move {
                let text = args.get("text").and_then(Value::as_str).unwrap_or("");
                ToolOutput::ok(format!("echo: {text}"))
            })
        }
    }

    /// Exec-tier fixture whose command text flows through the exec gate.
    struct RunnerHand;
    impl Hand for RunnerHand {
        fn def(&self) -> HandDef {
            HandDef {
                name: "runner".to_string(),
                description: "test fixture: exec tier".to_string(),
                parameters: json!({"type": "object"}),
                clearance: Clearance::Exec,
                read_only: false,
            }
        }

        fn execute<'a>(
            &'a self,
            _args: &'a Value,
            _ctx: &'a HandContext,
        ) -> std::pin::Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
            Box::pin(async move { ToolOutput::ok("ran".to_string()) })
        }
    }

    /// A fast-role speaker that always replies with one fixed label.
    struct LabelSpeaker(&'static str);
    impl ka_dialect::speaker::Speaker for LabelSpeaker {
        fn speak<'a>(
            &'a self,
            _req: SpeakRequest,
            out: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            Box::pin(async move {
                out.send(StreamEvent::Text(self.0.to_string())).await.ok();
                out.send(StreamEvent::Finished {
                    stop: Stop::Done,
                    usage: Usage::default(),
                })
                .await
                .ok();
            })
        }
    }

    fn hand_ctx() -> HandContext {
        HandContext {
            cwd: std::env::temp_dir(),
            ledger: Arc::new(parking_lot::Mutex::new(super::super::Ledger::default())),
            spill: Arc::new(super::super::Spill::new()),
            snapshots: Arc::new(parking_lot::Mutex::new(
                super::super::snapshots::Snapshots::inert(),
            )),
            jobs: Arc::new(super::super::jobs::JobTable::new()),
            bash_background_ms: 0,
            max_image_mb: 5,
            web_allow_private: false,
            sandbox: ka_sandbox::Policy::Off,
            shell_env: Default::default(),
        }
    }

    fn test_router(
        hands: Vec<Arc<dyn Hand>>,
        rules: Vec<Rule>,
        mode: Mode,
        disabled: Vec<ka_protocol::FeatureSpec>,
        fast_role: Option<AuxRole>,
    ) -> (Arc<CodeRouter>, tokio::sync::mpsc::Receiver<Event>) {
        let (events, rx) = tokio::sync::mpsc::channel(16);
        (
            Arc::new(CodeRouter::new(
                hands,
                rules,
                mode,
                PromptPolicy::Ask,
                Vec::new(),
                crate::features::FeatureToggles::slot(disabled),
                events,
                fast_role,
            )),
            rx,
        )
    }

    fn test_hand(router: Arc<CodeRouter>, limits: Code) -> CodeHand {
        CodeHand::new(router, limits)
    }

    async fn run(hand: &CodeHand, script: &str) -> ToolOutput {
        let ctx = hand_ctx();
        let args = json!({ "script": script });
        hand.execute(&args, &ctx).await
    }

    fn code_limits(timeout_ms: Option<u64>, memory_mb: Option<u32>) -> Code {
        Code {
            timeout_ms,
            memory_mb,
        }
    }

    fn fast_role(reply: &'static str) -> AuxRole {
        let catalog = ka_dialect::Catalog::parse(
            "[dialects.\"test/fast\"]\nwire = \"openai_chat\"\nbase_url = \
             \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let dialect = catalog.get("test/fast").unwrap().clone();
        AuxRole::resolve(
            "test/fast".to_string(),
            dialect,
            None,
            Arc::new(LabelSpeaker(reply)),
            crate::voice::Voice::ROLE_MAX_OUTPUT,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn arithmetic_and_json_results_return_to_the_model() {
        let (router, _rx) = test_router(Vec::new(), Vec::new(), Mode::Allow, Vec::new(), None);
        let hand = test_hand(router, code_limits(None, None));

        let out = run(&hand, "return (1 + 2) * 4").await;
        assert!(!out.is_error);
        assert_eq!(out.content, "12");

        // objects come back JSON-serialized
        let out = run(&hand, "return {a: 1, b: [true, null]}").await;
        assert_eq!(out.content, r#"{"a":1,"b":[true,null]}"#);

        // a bare return (undefined) comes back as the string "undefined"
        let out = run(&hand, "return").await;
        assert_eq!(out.content, "undefined");
    }

    #[tokio::test]
    async fn tool_call_binding_routes_and_awaits() {
        let (router, _rx) = test_router(
            vec![Arc::new(EchoHand)],
            Vec::new(),
            Mode::Allow,
            Vec::new(),
            None,
        );
        let hand = test_hand(router, code_limits(None, None));
        let out = run(
            &hand,
            "const v = await ka.tools.call(\"echo\", JSON.stringify({text: \"hi\"}));\n\
             return v;",
        )
        .await;
        // a returned string is JSON-serialized (quoted) like any value
        assert_eq!(out.content, "\"echo: hi\"");
    }

    #[tokio::test]
    async fn no_state_survives_between_invocations() {
        let (router, _rx) = test_router(Vec::new(), Vec::new(), Mode::Allow, Vec::new(), None);
        let hand = test_hand(router, code_limits(None, None));

        let out = run(
            &hand,
            "globalThis.marker = 42; return typeof globalThis.marker",
        )
        .await;
        assert_eq!(out.content, "\"number\"");

        let out = run(&hand, "return typeof globalThis.marker").await;
        assert_eq!(
            out.content, "\"undefined\"",
            "a fresh runtime per invocation"
        );
    }

    #[tokio::test]
    async fn denied_rule_resolves_to_an_error_string() {
        let (router, _rx) = test_router(
            vec![Arc::new(EchoHand)],
            vec![Rule {
                tool: "echo".to_string(),
                pattern: None,
                verdict: crate::config::Verdict::Deny,
            }],
            Mode::Allow,
            Vec::new(),
            None,
        );
        let hand = test_hand(router, code_limits(None, None));
        let out = run(
            &hand,
            "const v = await ka.tools.call(\"echo\", \"{}\");\n\
             const parsed = JSON.parse(v);\n\
             return parsed.error;",
        )
        .await;
        // the script resolves the refusal as data (the binding never
        // throws); the route prefixes every denial with "denied: "
        assert_eq!(out.content, "\"denied: rule for echo\"");
    }

    #[tokio::test]
    async fn allowed_tool_runs_and_disabled_toggle_refuses() {
        // allow: the fixture runs through the gate
        let (router, _rx) = test_router(
            vec![Arc::new(EchoHand)],
            Vec::new(),
            Mode::Allow,
            Vec::new(),
            None,
        );
        let hand = test_hand(router, code_limits(None, None));
        let out = run(
            &hand,
            "return await ka.tools.call(\"echo\", JSON.stringify({text: \"x\"}))",
        )
        .await;
        assert_eq!(out.content, "\"echo: x\"");

        // runtime toggle: the same call is refused by name, like the
        // admit path does for the model
        let (router, _rx) = test_router(
            vec![Arc::new(EchoHand)],
            Vec::new(),
            Mode::Allow,
            vec![ka_protocol::FeatureSpec::Tool("echo".into())],
            None,
        );
        let hand = test_hand(router, code_limits(None, None));
        let out = run(&hand, "return await ka.tools.call(\"echo\", \"{}\")").await;
        // the refusal is data (the script completed fine)
        assert!(
            out.content.contains("disabled by a session feature toggle"),
            "got: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn ask_class_gate_notes_the_user_and_denies_the_script() {
        let (router, mut rx) = test_router(
            vec![Arc::new(RunnerHand)],
            Vec::new(),
            Mode::Ask,
            Vec::new(),
            None,
        );
        let hand = test_hand(router, code_limits(None, None));
        let out = run(
            &hand,
            "return await ka.tools.call(\"runner\", JSON.stringify({command: \"make build\"}))",
        )
        .await;
        assert!(
            out.content.contains("denied: run `make build`?"),
            "got: {}",
            out.content
        );
        assert!(
            out.content
                .contains("cannot be answered from a running script"),
            "the denial says how to unblock: {}",
            out.content
        );
        // the user saw what was requested
        let note = rx.try_recv().expect("ask surfaced to the user");
        let ka_protocol::Event::Note { message } = note else {
            panic!("expected a Note event, got {note:?}");
        };
        assert!(message.contains("code script asked: run `make build`?"));
    }

    #[tokio::test]
    async fn hardstop_gate_denies_even_in_allow_mode() {
        let (router, mut rx) = test_router(
            vec![Arc::new(RunnerHand)],
            Vec::new(),
            Mode::Allow,
            Vec::new(),
            None,
        );
        let hand = test_hand(router, code_limits(None, None));
        let out = run(
            &hand,
            "return await ka.tools.call(\"runner\", JSON.stringify({command: \"rm -rf /\"}))",
        )
        .await;
        assert!(
            out.content.contains("denied: HARDSTOP"),
            "got: {}",
            out.content
        );
        assert!(rx.try_recv().is_ok(), "hardstop ask surfaced to the user");
    }

    #[tokio::test]
    async fn unknown_tool_resolves_to_an_error_string() {
        let (router, _rx) = test_router(Vec::new(), Vec::new(), Mode::Allow, Vec::new(), None);
        let hand = test_hand(router, code_limits(None, None));
        let out = run(&hand, "return await ka.tools.call(\"nope\", \"{}\")").await;
        assert!(
            out.content.contains("unknown tool nope"),
            "got: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn timeout_aborts_a_runaway_loop() {
        let (router, _rx) = test_router(Vec::new(), Vec::new(), Mode::Allow, Vec::new(), None);
        let hand = test_hand(router, code_limits(Some(150), Some(16)));
        let out = run(&hand, "while (true) {}").await;
        assert!(out.is_error);
        assert!(
            out.content.contains("timed out after 150 ms"),
            "got: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn memory_limit_aborts_runaway_allocation() {
        let (router, _rx) = test_router(Vec::new(), Vec::new(), Mode::Allow, Vec::new(), None);
        let hand = test_hand(router, code_limits(Some(10_000), Some(8)));
        let out = run(
            &hand,
            "const a = [];\nwhile (true) { a.push(new Array(4096).fill(\"x\")); }\nreturn a.length;",
        )
        .await;
        assert!(out.is_error);
        assert!(
            out.content.contains("memory limit (8 MB"),
            "got: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn syntax_error_reports_an_actionable_message() {
        let (router, _rx) = test_router(Vec::new(), Vec::new(), Mode::Allow, Vec::new(), None);
        let hand = test_hand(router, code_limits(None, None));
        // line 2 of the script, source line 3 inside the wrapper
        let out = run(&hand, "const x = 1;\nreturn (1 +").await;
        assert!(out.is_error);
        assert!(
            out.content.starts_with("code: uncaught exception:"),
            "got: {}",
            out.content
        );
        // the shifted frame points at the script's own line 3
        assert!(
            out.content.contains("eval_script:3"),
            "line hint present: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn classify_rejects_without_a_fast_role() {
        let (router, _rx) = test_router(Vec::new(), Vec::new(), Mode::Allow, Vec::new(), None);
        let hand = test_hand(router, code_limits(None, None));
        let out = run(
            &hand,
            "try {\n  await ka.models.classify(\"text\", [\"a\", \"b\"]);\n  return \"no error\";\n\
             } catch (e) {\n  return String(e.message);\n}",
        )
        .await;
        assert!(
            out.content.contains("no fast role configured"),
            "got: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn classify_resolves_label_and_confidence() {
        let (router, _rx) = test_router(
            Vec::new(),
            Vec::new(),
            Mode::Allow,
            Vec::new(),
            Some(fast_role("b")),
        );
        let hand = test_hand(router, code_limits(None, None));
        let out = run(
            &hand,
            "const r = await ka.models.classify(\"some text\", [\"a\", \"b\", \"c\"]);\n\
             return r;",
        )
        .await;
        assert!(
            out.content.contains("\"label\":\"b\"") && out.content.contains("\"confidence\":1"),
            "got: {}",
            out.content
        );

        // empty labels are refused before any model call
        let out = run(
            &hand,
            "try {\n  await ka.models.classify(\"text\", []);\n  return \"no error\";\n\
             } catch (e) {\n  return String(e.message);\n}",
        )
        .await;
        assert!(out.content.contains("labels must not be empty"));
    }

    #[tokio::test]
    async fn oversized_result_parks_in_a_spill_file() {
        let (router, _rx) = test_router(Vec::new(), Vec::new(), Mode::Allow, Vec::new(), None);
        let hand = test_hand(router, code_limits(None, None));
        let out = run(
            &hand,
            "let s = \"\";\nfor (let i = 0; i < 3000; i++) { s += \"0123456789\"; }\nreturn s;",
        )
        .await;
        assert!(!out.is_error);
        assert!(out.spill.is_some(), "30 KB result parked");
        assert!(
            out.content.contains("full value parked at"),
            "excerpt notes the spill: {}",
            &out.content[..out.content.len().min(400)]
        );
        assert!(
            out.content.len() <= super::RESULT_CAP + 200,
            "excerpt stays near the cap, got {} bytes",
            out.content.len()
        );
    }

    #[test]
    fn line_numbers_shift_to_script_coordinates() {
        assert_eq!(
            super::shift_line("    at foo (evalscript:3:7)"),
            "at foo (evalscript:2:7)"
        );
        // bare frames (no function name parens) shift too
        assert_eq!(
            super::shift_line("    at eval_script:4:1"),
            "at (eval_script:3:1)"
        );
        // a frame with no line:col pair stays untouched (verbatim)
        assert_eq!(
            super::shift_line("    at native (native)"),
            "    at native (native)"
        );
        // line 1 of the script (source line 2) never goes below 1
        assert_eq!(
            super::shift_line("at x (evalscript:1:1)"),
            "at x (evalscript:1:1)"
        );
    }
}
