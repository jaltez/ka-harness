use std::collections::{BTreeMap, BTreeSet};
use crate::spec::FlagSpec;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ParsedArgs {
    pub flags: BTreeSet<String>,
    pub values: BTreeMap<String, String>,
    pub positionals: Vec<String>,
}

/// BUGGY: misses `--key=val`, combined short flags `-abc`, `-oVal`, and `--` separator.
pub fn parse_args(spec: &FlagSpec, args: &[&str]) -> Result<ParsedArgs, String> {
    let mut out = ParsedArgs::default();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if let Some(long) = arg.strip_prefix("--") {
            let def = spec
                .find_long(long)
                .ok_or_else(|| format!("unknown flag --{long}"))?;
            if def.takes_value {
                i += 1;
                let val = args.get(i).ok_or_else(|| format!("missing value for --{long}"))?;
                out.values.insert(def.long.clone(), (*val).to_string());
            } else {
                out.flags.insert(def.long.clone());
            }
        } else if let Some(short) = arg.strip_prefix('-') {
            let ch = short.chars().next().ok_or("empty flag")?;
            let def = spec
                .find_short(ch)
                .ok_or_else(|| format!("unknown flag -{ch}"))?;
            if def.takes_value {
                i += 1;
                let val = args.get(i).ok_or_else(|| format!("missing value for -{ch}"))?;
                out.values.insert(def.long.clone(), (*val).to_string());
            } else {
                out.flags.insert(def.long.clone());
            }
        } else {
            out.positionals.push(arg.to_string());
        }
        i += 1;
    }
    Ok(out)
}
