use ronin_agent_core::*;
use serde_json::json;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn read_and_edit_tools_keep_workspace_contracts() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
    let read = create_read_only_tools(dir.path()).unwrap().remove(0);
    let result = read
        .execute(
            &json!({"path":"a.txt","offset":2}),
            &CancellationToken::new(),
        )
        .await;
    assert!(!result.is_error);
    assert!(result.result.contains("2: two"));
    let edit = create_mutation_tools(dir.path()).unwrap().remove(1);
    let result = edit
        .execute(
            &json!({"path":"a.txt","old_string":"two","new_string":"three"}),
            &CancellationToken::new(),
        )
        .await;
    assert!(!result.is_error);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "one\nthree\n"
    );
}

#[tokio::test]
async fn rejects_secret_and_parent_paths() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join(".env"), "TOKEN=secret").unwrap();
    let read = create_read_only_tools(dir.path()).unwrap().remove(0);
    assert!(
        read.execute(&json!({"path":".env"}), &CancellationToken::new())
            .await
            .is_error
    );
    let write = create_mutation_tools(dir.path()).unwrap().remove(0);
    assert!(
        write
            .execute(
                &json!({"path":"../escape","content":"x"}),
                &CancellationToken::new()
            )
            .await
            .is_error
    );
}

#[tokio::test]
async fn write_file_can_create_a_new_nonignored_file() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
    let write = create_mutation_tools(dir.path()).unwrap().remove(0);
    let created = write
        .execute(
            &json!({"path":"new.txt","content":"hello"}),
            &CancellationToken::new(),
        )
        .await;
    assert!(!created.is_error, "{}", created.result);
    let ignored = write
        .execute(
            &json!({"path":"ignored.txt","content":"no"}),
            &CancellationToken::new(),
        )
        .await;
    assert!(ignored.is_error);
}
