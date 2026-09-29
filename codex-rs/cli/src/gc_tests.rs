use super::*;
use pretty_assertions::assert_eq;

#[test]
fn duration_and_execution_boundaries() {
    for (text, days) in [("1d", 1), ("30d", 30), ("4w", 28), ("01d", 1)] {
        assert_eq!(parse_age(text), Ok(TimeDelta::days(days)));
    }
    for text in [
        "",
        "0d",
        "0w",
        "-1d",
        "+1d",
        "1.5d",
        "1h",
        "1D",
        "1d ",
        " 1d",
        "999999999999999999999999d",
        "9223372036854775807w",
    ] {
        assert!(parse_age(text).is_err(), "{text}");
    }
    assert!(GcCommand::try_parse_from(["gc", "--execute", "--dry-run"]).is_err());
    assert!(
        GcCommand::try_parse_from(["gc", "-e", "--older-than", "4w"])
            .unwrap()
            .execute
    );
    assert!(
        GcCommand::try_parse_from(["gc", "--execute"])
            .unwrap()
            .older_than
            .is_none()
    );
}

#[test]
fn preview_report_and_control_character_escaping() {
    assert_eq!(
        escape("a\nb\r\t\u{1b}[31m\u{202e}\u{2028}"),
        "a\\nb\\r\\t\\u{1b}[31m\\u{202e}\\u{2028}"
    );
    let report = render_report(
        &RetentionPlan::default(),
        Some(DateTime::from_timestamp_millis(/*millis*/ 1000).unwrap()),
        /*maintenance*/ 1048576,
        /*execute*/ false,
    );
    let report = report.replace("logical-size fallback", "allocated bytes");
    insta::assert_snapshot!(report, @r###"
    Preview (read-only)
    UTC cutoff (last update strictly before): 1970-01-01T00:00:01+00:00
    Would delete 0 conversations across 0 working directories (0 missing cwd).
    Estimated reclaimable conversation storage: 0.000000 GiB.
    Additional data-preserving maintenance: 1.000 MiB.
    Skipped: 0 active, 0 referenced, 0 otherwise ineligible.
    Working directories and project files are not deletion targets.
    File estimates use allocated bytes; retained hard links are excluded. SQLite estimates exclude its retained reserve and are counted once. Actual disk-space gains can differ.
    "###);
}
