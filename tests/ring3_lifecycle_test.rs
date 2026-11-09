#[path = "support/evolution.rs"]
mod fixture;
use fixture::Fixture;

#[test]
fn observed_failures_promote_synthesize_reload_and_attribute() {
    let f = Fixture::new(
        "[model]\nclass=\"frontier\"\ninject_templates=false\n[evolution]\nattribution_holdout_modulus=0\n",
    );
    // Keep the default two-round promotion policy: one bad round alone must
    // not publish a skill. All observations are synthetic tool outcomes.
    for round in 0..2 {
        for attempt in 0..6 {
            let output = f.call(&["observe"], serde_json::json!({"hook_event_name":"PostToolUse","session_id":format!("failed-{round}"),"tool_name":"Bash","tool_input":{"command":format!("node fixture.js --round {round} --attempt {attempt}")},"tool_response":{"output":"TypeError: synthetic lifecycle failure","exit_code":1}}));
            assert!(output.status.success());
        }
        let output = f.call(&["reflect"], serde_json::json!({"hook_event_name":"Stop"}));
        assert!(output.status.success());
        if round == 0 {
            assert!(
                !f.project.join("pending_synth.jsonl").exists(),
                "A single round must not bypass promotion"
            );
        }
    }
    let pending: Vec<serde_json::Value> =
        std::fs::read_to_string(f.project.join("pending_synth.jsonl"))
            .expect("Two eligible rounds should produce pending synthesis")
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
    let skill = pending
        .iter()
        .find(|record| record["status"] == "pending")
        .expect("A skill must be pending")["skill_name"]
        .as_str()
        .unwrap();
    let before = f.call(&["resume"], serde_json::json!({"hook_event_name":"SessionStart","session_id":"before-synthesis","source":"startup"}));
    assert!(
        !fixture::context(&before).contains(&format!("### {skill}\n")),
        "Frontier policy must withhold the unsynthesized template"
    );
    let body = f.work.join("synthesized.md");
    fixture::write(
        &body,
        "## Process\nRING3_LIFECYCLE_SENTINEL: read the exact TypeError and inspect its value before editing. Add a focused regression and verify the failing path.\n## Red Flags\nDo not retry a command without examining its diagnostic.\n",
    );
    let accepted = f.call(
        &[
            "evolve",
            "accept-synth",
            "--skill",
            skill,
            "--file",
            body.to_str().unwrap(),
        ],
        serde_json::json!({}),
    );
    assert!(
        accepted.status.success(),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
    let resumed = f.call(&["resume"], serde_json::json!({"hook_event_name":"SessionStart","session_id":"treatment","source":"startup"}));
    assert!(fixture::context(&resumed).contains("RING3_LIFECYCLE_SENTINEL"));
    for attempt in 0..3 {
        f.call(&["observe"], serde_json::json!({"hook_event_name":"PostToolUse","session_id":"treatment","tool_name":"Bash","tool_input":{"command":format!("node fixture.js --success {attempt}")},"tool_response":{"output":"synthetic passed","exit_code":0}}));
    }
    f.call(
        &["reflect"],
        serde_json::json!({"hook_event_name":"Stop","session_id":"treatment"}),
    );
    let metrics = f.metric();
    assert_eq!(metrics["total_sessions"], 3);
    assert_eq!(metrics["skill_attribution"][skill]["sessions_active"], 1);
    f.call(
        &["reflect"],
        serde_json::json!({"hook_event_name":"Stop","session_id":"treatment"}),
    );
    assert_eq!(
        f.metric()["total_sessions"],
        3,
        "Repeated Stop must not score consumed observations again"
    );
}
