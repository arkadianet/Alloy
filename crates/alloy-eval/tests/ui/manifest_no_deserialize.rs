//! Compile-fail: validated manifest types must not implement Deserialize.
fn require_deserialize<T: serde::de::DeserializeOwned>() {}

fn main() {
    require_deserialize::<alloy_eval::FixtureManifest>();
    require_deserialize::<alloy_eval::ScriptTurn>();
    require_deserialize::<alloy_eval::ScriptTurnOutcome>();
}
