use ronin_agent_core::*;
use serde_json::Value;

#[test]
fn agent_contract_fixture_deserializes() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/agent-contract.json")).unwrap();
    for message in fixture["messages"].as_array().unwrap() {
        serde_json::from_value::<AgentMessage>(message.clone()).unwrap();
    }
    for event in fixture["events"].as_array().unwrap() {
        serde_json::from_value::<GenerationStreamEvent>(event.clone()).unwrap();
    }
    serde_json::from_value::<ModelSummary>(fixture["model"].clone()).unwrap();
    serde_json::from_value::<BalanceSummary>(fixture["balance"].clone()).unwrap();
}
