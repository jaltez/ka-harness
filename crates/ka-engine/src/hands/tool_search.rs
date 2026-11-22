//! The `tool_search` hand: keyword search over everything the model
//! could call — the hands registered in this session and every
//! connected MCP server's catalog (server name + tool name +
//! description). Mostly valuable when MCP runs lazy or a server
//! exposes dozens of tools: it tells the model WHAT exists and HOW to
//! invoke it (`mcp_call` for lazy MCP, the bare hand name otherwise)
//! without flooding the tool list. Registered always — with no MCP it
//! still answers over the built-in hands.

use std::sync::Arc;

use parking_lot::RwLock;
use serde_json::{Value, json};

use super::{Clearance, Hand, HandContext, HandDef, ToolOutput};

/// One searchable hand entry (name + description + schema), refreshed
/// by the voice whenever its model-facing tool spec set rebuilds.
#[derive(Debug, Clone)]
pub struct HandCatalogEntry {
    pub name: String,
    pub description: String,
}

/// Searchable hand registry: (name, description) rows.
pub type HandCatalog = Arc<RwLock<Vec<HandCatalogEntry>>>;

/// Searchable MCP catalogs: one handle per connected server.
pub type McpCatalog = Arc<RwLock<Vec<crate::mcp::McpShared>>>;

/// Default match cap.
const DEFAULT_LIMIT: usize = 8;
/// Match cap the caller can raise to.
const MAX_LIMIT: usize = 32;

/// One scored match row.
struct MatchRow {
    score: u32,
    line: String,
}

/// Case-insensitive keyword scoring: name hits outrank description
/// hits; every additional keyword occurrence adds a little.
fn score(text: &str, query_lower: &str) -> u32 {
    let t = text.to_lowercase();
    let mut n = 0u32;
    let mut rest = t.as_str();
    while let Some(i) = rest.find(query_lower) {
        n += 1;
        rest = &rest[i + query_lower.len()..];
    }
    n
}

/// The tool_search tool.
pub struct ToolSearchHand {
    hands: HandCatalog,
    mcp: McpCatalog,
}

impl ToolSearchHand {
    /// Hand over the shared catalogs (the voice keeps them fresh).
    pub fn new(hands: HandCatalog, mcp: McpCatalog) -> Self {
        Self { hands, mcp }
    }
}

impl Hand for ToolSearchHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "tool_search".to_string(),
            description: "Search the tools you can call, by keyword: the built-in hands plus \
                every connected MCP server's catalog (even when MCP discovery is lazy and the \
                individual tools are not in your tool list). Arguments: {\"query\": \
                \"deploy\", \"limit\": 8}. Returns ranked `tool — description` lines with a \
                hint on how to invoke each match (call a hand by name; call MCP tools via \
                `mcp_call` with {server, tool, arguments})."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Keyword(s) to search names and descriptions"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max matches (default 8, max 32)"
                    }
                },
                "required": ["query"]
            }),
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
            let Some(query) = args.get("query").and_then(Value::as_str) else {
                return ToolOutput::err("tool_search: missing required 'query'");
            };
            let query = query.trim();
            if query.is_empty() {
                return ToolOutput::err("tool_search: 'query' must not be empty");
            }
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .map(|l| l.clamp(1, MAX_LIMIT as u64) as usize)
                .unwrap_or(DEFAULT_LIMIT);
            ToolOutput::ok(self.search(query, limit))
        })
    }
}

impl ToolSearchHand {
    /// Rank hands and MCP tools against `query`; render the top
    /// `limit` with per-match invocation hints.
    fn search(&self, query: &str, limit: usize) -> String {
        let query_lower = query.to_lowercase();
        // hands: name hits weigh double
        let mut rows: Vec<MatchRow> = Vec::new();
        for h in self.hands.read().iter() {
            let s = 2 * score(&h.name, &query_lower) + score(&h.description, &query_lower);
            if s > 0 {
                rows.push(MatchRow {
                    score: s,
                    line: format!(
                        "{} — {}  [invoke: call the `{}` tool directly]",
                        h.name,
                        one_line(&h.description),
                        h.name
                    ),
                });
            }
        }
        // MCP catalogs: server.tool rows with the lazy/eager invocation hint
        for server in self.mcp.read().iter() {
            let server_name = server.name().to_string();
            for tool in server.tools() {
                let s = 2 * score(&tool.raw_name, &query_lower)
                    + 2 * score(&tool.name, &query_lower)
                    + score(&tool.description, &query_lower);
                if s > 0 {
                    rows.push(MatchRow {
                        score: s,
                        line: format!(
                            "{}.{} — {}  [invoke: mcp_call {{\"server\": \"{}\", \"tool\": \
                             \"{}\", \"arguments\": {{…}}}}]",
                            server_name,
                            tool.raw_name,
                            one_line(&tool.description),
                            server_name,
                            tool.raw_name,
                        ),
                    });
                }
            }
        }
        rows.sort_by_key(|a| std::cmp::Reverse(a.score));
        if rows.is_empty() {
            return format!("no tools match '{query}'");
        }
        let mut out = format!(
            "{} match(es) for '{query}' (top {}):\n",
            rows.len(),
            rows.len().min(limit)
        );
        for row in rows.into_iter().take(limit) {
            out.push_str(&row.line);
            out.push('\n');
        }
        out
    }
}

/// Collapse a description to its first line (schema blobs must not
/// flood the search result).
fn one_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    let mut out: String = line.chars().take(200).collect();
    if line.chars().count() > 200 {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn hand_with(hands: Vec<(String, String)>) -> ToolSearchHand {
        let hand_catalog: HandCatalog = Arc::new(RwLock::new(
            hands
                .into_iter()
                .map(|(name, description)| HandCatalogEntry { name, description })
                .collect(),
        ));
        ToolSearchHand {
            hands: hand_catalog,
            mcp: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// A search hand over one synthetic MCP server catalog (a
    /// disconnected placeholder — search only reads the tool list).
    fn hand_with_mcp(
        hands: Vec<(String, String)>,
        server: &str,
        tools: Vec<(&str, &str)>,
    ) -> ToolSearchHand {
        let hand_catalog: HandCatalog = Arc::new(RwLock::new(
            hands
                .into_iter()
                .map(|(name, description)| HandCatalogEntry { name, description })
                .collect(),
        ));
        let tools = tools
            .into_iter()
            .map(|(raw, description)| crate::mcp::McpTool {
                name: format!("{server}.{raw}"),
                raw_name: raw.to_string(),
                description: description.to_string(),
                schema: json!({}),
                read_only_hint: false,
            })
            .collect();
        let shared = crate::mcp::McpShared::placeholder(
            crate::mcp::McpServerConfig {
                name: server.to_string(),
                command: None,
                args: Vec::new(),
                env: Default::default(),
                url: None,
                headers: Vec::new(),
                enabled_tools: Vec::new(),
                disabled_tools: Vec::new(),
                approval: None,
                tool_timeout_secs: None,
                output_limit: None,
            },
            tools,
        );
        ToolSearchHand {
            hands: hand_catalog,
            mcp: Arc::new(RwLock::new(vec![shared])),
        }
    }

    #[tokio::test]
    async fn searches_over_registered_hands() {
        let hand = hand_with(vec![
            ("read".into(), "Read a file from disk.".into()),
            ("bash".into(), "Run a shell command.".into()),
            ("web_search".into(), "Search the web.".into()),
        ]);
        let out = hand.execute(&json!({"query": "search"}), &test_ctx()).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("web_search"), "{}", out.content);
        assert!(!out.content.contains("Read a file"), "{}", out.content);
        assert!(out.content.contains("invoke"), "{}", out.content);
    }

    #[tokio::test]
    async fn ranks_name_hits_above_description_hits() {
        let hand = hand_with(vec![
            ("grep".into(), "Search file contents.".into()), // description hit only
            ("search".into(), "totally unrelated".into()),   // name hit
        ]);
        let out = hand.execute(&json!({"query": "search"}), &test_ctx()).await;
        let name_hit = out.content.find("search — totally unrelated");
        let desc_hit = out.content.find("grep — Search file contents");
        let (a, b) = (name_hit.unwrap(), desc_hit.unwrap());
        assert!(a < b, "name hits must rank first:\n{}", out.content);
    }

    #[tokio::test]
    async fn no_match_and_empty_query_error() {
        let hand = hand_with(vec![("read".into(), "Read a file.".into())]);
        let out = hand
            .execute(&json!({"query": "kubernetes"}), &test_ctx())
            .await;
        assert!(!out.is_error);
        assert!(out.content.contains("no tools match"), "{}", out.content);
        let missing = hand.execute(&json!({}), &test_ctx()).await;
        assert!(missing.is_error);
        let empty = hand.execute(&json!({"query": "   "}), &test_ctx()).await;
        assert!(empty.is_error);
    }

    #[tokio::test]
    async fn limit_caps_results() {
        let hands: Vec<(String, String)> = (0..10)
            .map(|i| (format!("deploy{i}"), "deploy the service".to_string()))
            .collect();
        let hand = hand_with(hands);
        let out = hand
            .execute(&json!({"query": "deploy", "limit": 3}), &test_ctx())
            .await;
        let matches = out.content.lines().filter(|l| l.contains(" — ")).count();
        assert_eq!(matches, 3, "{}", out.content);
    }

    #[tokio::test]
    async fn mcp_catalog_rows_show_invocation_hint() {
        let hand = hand_with_mcp(
            vec![("read".into(), "Read a file from disk.".into())],
            "github",
            vec![
                ("create_issue", "Open an issue on a repository."),
                ("list_prs", "List pull requests."),
            ],
        );
        let out = hand.execute(&json!({"query": "issue"}), &test_ctx()).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("github.create_issue"),
            "{}",
            out.content
        );
        assert!(out.content.contains("mcp_call"), "{}", out.content);
        assert!(!out.content.contains("list_prs"), "{}", out.content);
        // hands that match stay listed alongside MCP tools
        let out2 = hand.execute(&json!({"query": "file"}), &test_ctx()).await;
        assert!(out2.content.contains("read —"), "{}", out2.content);
    }

    #[tokio::test]
    async fn empty_catalog_says_no_matches_not_error() {
        // the always-registered hand stays cheap when nothing is connected
        let hand = hand_with(vec![]);
        let out = hand
            .execute(&json!({"query": "anything"}), &test_ctx())
            .await;
        assert!(!out.is_error);
        assert!(out.content.contains("no tools match"));
    }

    /// Minimal HandContext for execute (nothing here reads it).
    fn test_ctx() -> HandContext {
        HandContext {
            cwd: std::env::temp_dir(),
            ledger: std::sync::Arc::new(parking_lot::Mutex::new(super::super::Ledger::default())),
            spill: std::sync::Arc::new(super::super::Spill::new()),
            snapshots: std::sync::Arc::new(parking_lot::Mutex::new(
                super::super::snapshots::Snapshots::inert(),
            )),
            jobs: std::sync::Arc::new(super::super::jobs::JobTable::new()),
            bash_background_ms: 0,
            max_image_mb: 5,
            web_allow_private: false,
            sandbox: ka_sandbox::Policy::Off,
            shell_env: Default::default(),
        }
    }
}
