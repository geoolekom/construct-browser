use std::io;
use std::path::{Path, PathBuf};

use crate::codex::{Sessions, find_sessions, open_session, parse_session_line, sessions_root};
use crate::{FileState, Listing, Parsed, Tool};

/// The ChatGPT app: its chats without a project. The app writes each chat as a Codex session
/// file, under the sessions directory of [`Codex`](crate::Codex), with a folder it makes for the
/// chat in its own workspace (`Documents/Codex`). That folder is not a project, so no record
/// carries it. A session with a project chosen in the app is left to Codex.
pub struct ChatGpt;

impl Tool for ChatGpt {
    fn name(&self) -> &'static str {
        Sessions::ChatGpt.tool()
    }

    fn root(&self, home: &Path) -> PathBuf {
        sessions_root(home)
    }

    fn find_files(&self, root: &Path) -> io::Result<Listing> {
        find_sessions(root, Sessions::ChatGpt)
    }

    fn open_file(&self, path: &Path) -> FileState {
        open_session(path)
    }

    fn parse_line(&self, state: &mut FileState, line: &str) -> Parsed {
        parse_session_line(Sessions::ChatGpt, state, line)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::codex::BELONGS;
    use crate::codex::tests::{item, message, turn};
    use crate::{Author, Codex, Role};

    const CHAT: &str = "01a20dea-9b83-7943-8e6b-22aa40b51e64";
    const PROJECT: &str = "01a20df8-5beb-7620-845a-d017d2bc0732";
    const CLI: &str = "01a0bee2-6260-73a3-b389-43c86bb8930f";
    const CHAT_FOLDER: &str = "/Users/p/Documents/Codex/2026-11-24/2864";

    fn meta(id: &str, fields: &str) -> String {
        item(
            "session_meta",
            &format!(r#"{{"id":"{id}","base_instructions":{{"text":"long"}},{fields}}}"#),
        )
    }

    fn chat_meta() -> String {
        meta(
            CHAT,
            &format!(
                r#""originator":"Codex Desktop","source":"vscode","cwd":"{CHAT_FOLDER}","runtime_workspace_roots":["/Users/p/Documents/Codex"]"#
            ),
        )
    }

    fn rollout(id: &str) -> String {
        format!("rollout-2026-11-24T16-16-48-{id}.jsonl")
    }

    /// A sessions directory with one day, made fresh for the test `name`.
    fn day(name: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("chatgpt-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let day = root.join("2026/11/24");
        fs::create_dir_all(&day).unwrap();
        (root, day)
    }

    fn names(listing: &Listing) -> Vec<String> {
        listing
            .files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    /// The sessions of `metas`, one file each, as the two readers split them.
    fn split(name: &str, metas: &[(&str, String)]) -> (Vec<String>, Vec<String>) {
        let (root, day) = day(name);
        for (id, meta) in metas {
            fs::write(day.join(rollout(id)), format!("{meta}\n")).unwrap();
        }
        let chats = names(&ChatGpt.find_files(&root).unwrap());
        let others = names(&Codex.find_files(&root).unwrap());
        let _ = fs::remove_dir_all(root);
        (chats, others)
    }

    #[test]
    fn a_chat_of_the_app_is_chatgpt_and_every_other_session_stays_codex() {
        let project = meta(
            PROJECT,
            r#""originator":"Codex Desktop","source":"vscode","cwd":"/w/app","runtime_workspace_roots":["/w/app"]"#,
        );
        let cli = meta(
            CLI,
            r#""originator":"codex_cli_rs","source":"cli","cwd":"/w/app""#,
        );
        let (chats, others) = split(
            "split",
            &[(CHAT, chat_meta()), (PROJECT, project), (CLI, cli)],
        );
        assert_eq!(chats, [rollout(CHAT)]);
        assert_eq!(others, [rollout(CLI), rollout(PROJECT)]);
    }

    #[test]
    fn a_session_that_may_work_outside_the_workspace_or_in_the_workspace_itself_stays_codex() {
        // A project next to the workspace.
        let both = meta(
            CHAT,
            &format!(
                r#""originator":"Codex Desktop","cwd":"{CHAT_FOLDER}","runtime_workspace_roots":["/Users/p/Documents/Codex","/w/app"]"#
            ),
        );
        // The workspace itself chosen as the project.
        let workspace = meta(
            PROJECT,
            r#""originator":"Codex Desktop","cwd":"/Users/p/Documents/Codex","runtime_workspace_roots":["/Users/p/Documents/Codex"]"#,
        );
        let (chats, others) = split("outside", &[(CHAT, both), (PROJECT, workspace)]);
        assert!(chats.is_empty(), "{chats:?}");
        assert_eq!(others, [rollout(CHAT), rollout(PROJECT)]);
    }

    #[test]
    fn without_the_list_of_workspaces_the_folder_of_the_session_decides() {
        let in_workspace = meta(
            CHAT,
            &format!(r#""originator":"Codex Desktop","cwd":"{CHAT_FOLDER}""#),
        );
        // The same folder from another app is not a chat of the ChatGPT app.
        let other_app = meta(
            CLI,
            &format!(r#""originator":"codex_cli_rs","cwd":"{CHAT_FOLDER}""#),
        );
        // The workspace itself, chosen as a project, is not a chat folder.
        let workspace = meta(
            PROJECT,
            r#""originator":"Codex Desktop","cwd":"/Users/p/Documents/Codex""#,
        );
        let (chats, others) = split(
            "by-folder",
            &[(CHAT, in_workspace), (CLI, other_app), (PROJECT, workspace)],
        );
        assert_eq!(chats, [rollout(CHAT)]);
        assert_eq!(others, [rollout(CLI), rollout(PROJECT)]);
    }

    #[test]
    fn a_file_without_a_complete_first_line_is_read_by_neither_until_the_line_is_whole() {
        let (root, day) = day("later");
        let path = day.join(rollout(CHAT));
        let line = chat_meta();
        fs::write(&path, &line[..40]).unwrap();
        assert!(ChatGpt.find_files(&root).unwrap().files.is_empty());
        assert!(Codex.find_files(&root).unwrap().files.is_empty());

        fs::write(&path, line + "\n").unwrap();
        assert_eq!(ChatGpt.find_files(&root).unwrap().files, [path]);
        assert!(Codex.find_files(&root).unwrap().files.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_file_replaced_by_a_shorter_one_at_the_same_path_is_known_again_and_a_gone_one_forgotten() {
        let (root, day) = day("replaced");
        let path = day.join(rollout(CHAT));
        fs::write(&path, format!("{}\n", chat_meta())).unwrap();
        assert_eq!(
            ChatGpt.find_files(&root).unwrap().files,
            std::slice::from_ref(&path)
        );
        let cli = meta(CHAT, r#""originator":"codex_cli_rs","cwd":"/w""#);
        fs::write(&path, format!("{cli}\n")).unwrap();
        assert!(ChatGpt.find_files(&root).unwrap().files.is_empty());
        assert_eq!(
            Codex.find_files(&root).unwrap().files,
            std::slice::from_ref(&path)
        );

        fs::remove_file(&path).unwrap();
        assert!(Codex.find_files(&root).unwrap().files.is_empty());
        let known = BELONGS.lock().unwrap().contains_key(&path);
        assert!(!known, "a file no listing finds is forgotten");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_chat_is_read_as_chatgpt_without_its_folder() {
        let path = PathBuf::from(format!("/h/.codex/sessions/2026/11/24/{}", rollout(CHAT)));
        let mut st = ChatGpt.open_file(&path);
        assert_eq!(ChatGpt.parse_line(&mut st, &chat_meta()), Parsed::Ignored);
        let context = item("turn_context", &format!(r#"{{"cwd":"{CHAT_FOLDER}"}}"#));
        assert_eq!(ChatGpt.parse_line(&mut st, &context), Parsed::Ignored);

        let u = turn(ChatGpt.parse_line(&mut st, &message("user", "input_text", "explain")));
        assert_eq!(
            (u.tool.as_str(), u.session.as_str(), u.folder.as_str()),
            ("chatgpt", CHAT, "")
        );
        assert_eq!(
            (u.role, u.author, u.text.as_str()),
            (Role::User, Author::Person, "explain")
        );
        let a = turn(ChatGpt.parse_line(&mut st, &message("assistant", "output_text", "It is")));
        assert_eq!((a.tool.as_str(), a.folder.as_str()), ("chatgpt", ""));
    }

    #[test]
    fn a_chat_read_on_from_the_state_codex_saved_drops_the_folder() {
        let mut st = FileState {
            session: CHAT.into(),
            folder: CHAT_FOLDER.into(),
            author: Author::Person,
            ..FileState::default()
        };
        let u = turn(ChatGpt.parse_line(&mut st, &message("user", "input_text", "one more")));
        assert_eq!(
            (u.tool.as_str(), u.session.as_str(), u.folder.as_str()),
            ("chatgpt", CHAT, "")
        );
        assert_eq!(st.folder, "");
    }

    #[test]
    fn the_root_is_the_one_of_codex() {
        let home = Path::new("/h");
        assert_eq!(ChatGpt.root(home), Codex.root(home));
    }
}
