use super::*;
use pretty_assertions::assert_eq;

#[test]
fn recent_models_retain_distinct_choices_and_each_effort_across_restarts() {
    let home = tempfile::tempdir().unwrap();
    let mut history = RecentModels::load(home.path());
    for (model, effort) in [
        ("gpt-6.1-sol", ReasoningEffort::High),
        ("gpt-6-astra", ReasoningEffort::Max),
        ("gpt-6-astra", ReasoningEffort::Ultra),
    ] {
        assert!(history.remember(model, Some(effort)));
    }
    let sol = RecentModel {
        model: "gpt-6.1-sol".into(),
        effort: Some(ReasoningEffort::High),
    };
    let astra = RecentModel {
        model: "gpt-6-astra".into(),
        effort: Some(ReasoningEffort::Ultra),
    };
    assert_eq!(history.entries, [Some(astra.clone()), Some(sol.clone())]);
    assert!(!history.remember(&astra.model, astra.effort.clone()));
    history.save(home.path()).unwrap();
    let mut restored = RecentModels::load(home.path());
    assert_eq!(restored, history);
    restored.remember(&sol.model, sol.effort.clone());
    assert_eq!(restored.entries, [Some(sol), Some(astra)]);
}

#[test]
fn recent_models_ignore_corrupt_or_oversized_history() {
    let home = tempfile::tempdir().unwrap();
    for content in [
        "broken".to_string(),
        format!(
            r#"{{"entries":[{{"model":"{}","effort":null}},null]}}"#,
            "x".repeat(/*n*/ 4096)
        ),
    ] {
        std::fs::write(home.path().join(HISTORY_FILE), content).unwrap();
        assert_eq!(RecentModels::load(home.path()), RecentModels::default());
    }
}
