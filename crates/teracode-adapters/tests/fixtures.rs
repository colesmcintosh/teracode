use teracode_adapters::parse_normalized_event;
use teracode_core::{AdapterKind, AgentEvent};

#[test]
fn recorded_provider_streams_are_forward_compatible() {
    let fixtures = [
        (AdapterKind::Claude, include_str!("fixtures/claude.jsonl")),
        (AdapterKind::Codex, include_str!("fixtures/codex.jsonl")),
        (
            AdapterKind::OpenCode,
            include_str!("fixtures/opencode.jsonl"),
        ),
        (AdapterKind::Cursor, include_str!("fixtures/cursor.jsonl")),
        (AdapterKind::Grok, include_str!("fixtures/grok.jsonl")),
        (AdapterKind::Droid, include_str!("fixtures/droid.jsonl")),
    ];

    for (adapter, fixture) in fixtures {
        let events = fixture
            .lines()
            .map(|line| parse_normalized_event(adapter, line))
            .collect::<Vec<_>>();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::Completed { .. })),
            "{adapter} fixture has no completion"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                AgentEvent::Diagnostic { .. } | AgentEvent::Usage { .. }
            )),
            "{adapter} fixture has no forward-compatibility event"
        );
        for event in events {
            if let AgentEvent::Diagnostic { raw, .. } = event {
                assert!(!raw.contains("must-not-leak"));
            }
        }
    }
}
