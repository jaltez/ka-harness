use cli_flags::{parse_args, FlagSpec};

fn make_spec() -> FlagSpec {
    FlagSpec::new()
        .flag("verbose", Some('v'), false)
        .flag("all", Some('a'), false)
        .flag("force", Some('f'), false)
        .flag("output", Some('o'), true)
        .flag("format", Some('t'), true)
}

#[test]
fn long_equals_syntax() {
    let parsed = parse_args(&make_spec(), &["--output=out.txt", "--format=json", "in.txt"]).unwrap();
    assert_eq!(parsed.values.get("output").map(String::as_str), Some("out.txt"));
    assert_eq!(parsed.values.get("format").map(String::as_str), Some("json"));
    assert_eq!(parsed.positionals, vec!["in.txt"]);
}

#[test]
fn combined_short_flags_and_attached_value() {
    let parsed = parse_args(&make_spec(), &["-vaf", "-oresult.bin"]).unwrap();
    assert!(parsed.flags.contains("verbose"));
    assert!(parsed.flags.contains("all"));
    assert!(parsed.flags.contains("force"));
    assert_eq!(parsed.values.get("output").map(String::as_str), Some("result.bin"));
}

#[test]
fn combined_short_flags_ending_in_value_flag() {
    let parsed = parse_args(&make_spec(), &["-vo", "file.txt"]).unwrap();
    assert!(parsed.flags.contains("verbose"));
    assert_eq!(parsed.values.get("output").map(String::as_str), Some("file.txt"));
}

#[test]
fn double_dash_ends_options() {
    let parsed = parse_args(&make_spec(), &["-v", "--", "--output=not-a-flag", "-a"]).unwrap();
    assert!(parsed.flags.contains("verbose"));
    assert!(!parsed.flags.contains("all"));
    assert!(parsed.values.is_empty());
    assert_eq!(parsed.positionals, vec!["--output=not-a-flag", "-a"]);
}

#[test]
fn single_dash_is_positional() {
    let parsed = parse_args(&make_spec(), &["-v", "-"]).unwrap();
    assert!(parsed.flags.contains("verbose"));
    assert_eq!(parsed.positionals, vec!["-"]);
}
