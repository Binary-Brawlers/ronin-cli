#[cfg(test)]
mod tests {
    use crate::{config::load_config, storage::*};
    use ronin_agent_core::AgentMessage;
    use tempfile::tempdir;

    #[test]
    fn config_layers_global_then_project() {
        let home = tempdir().unwrap();
        let cwd = tempdir().unwrap();
        std::fs::create_dir(home.path().join(".ronin")).unwrap();
        std::fs::write(
            home.path().join(".ronin/config.toml"),
            "api_url='https://global.example'\nmax_rounds=10\n",
        )
        .unwrap();
        std::fs::write(cwd.path().join("ronin.toml"), "max_rounds=20\n").unwrap();
        let value = load_config(cwd.path(), home.path()).unwrap();
        assert_eq!(value.api_url, "https://global.example");
        assert_eq!(value.max_rounds, 20);
    }

    #[test]
    fn reads_v1_session_and_writes_v2() {
        let home = tempdir().unwrap();
        let store = SessionStore::new(home.path());
        let mut session = store
            .create(home.path(), "model", vec![AgentMessage::user("hello")])
            .unwrap();
        let path = home
            .path()
            .join(".ronin/sessions")
            .join(format!("{}.json", session.id));
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        value["version"] = 1.into();
        value.as_object_mut().unwrap().remove("compactionCount");
        value.as_object_mut().unwrap().remove("modelHistory");
        std::fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();
        session = store.load(&session.id).unwrap();
        assert_eq!(session.version, 3);
        assert_eq!(session.model_history, vec!["model"]);
    }
}
