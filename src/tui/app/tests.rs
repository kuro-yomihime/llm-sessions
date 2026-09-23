use super::*;
use crate::model::{Block, Liveness, Locator, PathTrust, Role, Turn};
use crate::output::reader::{LineKind, Rail, wrap_rendered};

fn session(source: SourceId, id: &str, workspace: &str, secs: i64) -> SessionSummary {
    SessionSummary {
        id: id.to_string(),
        source,
        workspace: (!workspace.is_empty()).then(|| PathBuf::from(workspace)),
        recorded_cwd: (!workspace.is_empty()).then(|| PathBuf::from(workspace)),
        trust: PathTrust::Recorded,
        title: Some(format!("title {id}")),
        created: None,
        updated: OffsetDateTime::from_unix_timestamp(secs).unwrap(),
        model: None,
        parent: None,
        archived: false,
        usage: None,
        size_bytes: None,
        locator: Locator::File {
            path: PathBuf::from("/nonexistent"),
        },
        branch: None,
        subagent: false,
        change: None,
        liveness: Liveness::Stale,
        host: None,
    }
}

/// Sessions dated relative to now, so recency-sensitive code behaves the
/// same on every run.
fn recent(source: SourceId, id: &str, workspace: &str, days_ago: i64) -> SessionSummary {
    let mut s = session(source, id, workspace, 0);
    s.updated = OffsetDateTime::now_utc() - time::Duration::days(days_ago);
    s
}

fn app_with(sessions: Vec<SessionSummary>) -> App {
    App::new(
        Path::new("/home/u"),
        Vec::new(),
        sessions,
        Vec::new(),
        None,
        None,
        Fleet::default(),
    )
}

fn press(app: &mut App, code: crossterm::event::KeyCode) {
    let key = crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
    if let Some(action) = crate::tui::keymap::action_for_key(key, app.prompt()) {
        app.handle(action);
    }
}

#[test]
fn ctrl_c_exits_every_modal_without_archiving() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    for prompt in [
        Prompt::None,
        Prompt::Filter("unfinished".into()),
        Prompt::Search("unfinished".into()),
        Prompt::Resize,
        Prompt::Fold,
        Prompt::ConfirmArchive {
            count: 1,
            groups: 0,
            archive: true,
        },
    ] {
        let mut app = app_with(vec![recent(SourceId::Omp, "a", "/w", 1)]);
        app.prompt = prompt;
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let action = crate::tui::keymap::action_for_key(key, app.prompt()).unwrap();
        app.handle(action);
        assert!(app.should_quit(), "{:?}", app.prompt());
        assert!(!app.selected_session().unwrap().archived);
    }
    let mut app = app_with(Vec::new());
    app.handle(Action::ToggleHelp);
    app.handle(Action::Quit);
    assert!(app.should_quit());
}

#[test]
fn prompt_typing_and_confirmation_do_not_leak_navigation() {
    use crossterm::event::KeyCode;
    let mut app = app_with(vec![recent(SourceId::Omp, "a", "/w", 1)]);
    app.handle(Action::StartFilter);
    for c in "q/vJ".chars() {
        press(&mut app, KeyCode::Char(c));
    }
    assert_eq!(app.prompt(), &Prompt::Filter("q/vJ".into()));
    assert!(!app.should_quit());
    assert_eq!(app.focus(), Pane::Groups);
    press(&mut app, KeyCode::Esc);
    app.handle(Action::ArchiveMarked);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Tab);
    assert!(!app.selected_session().unwrap().archived);
    assert!(matches!(app.prompt(), Prompt::ConfirmArchive { .. }));
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(app.prompt(), &Prompt::None);
    assert!(!app.selected_session().unwrap().archived);
}

/// `q` sits where `Esc` does in muscle memory, and killing the program by
/// reflex is not a recoverable mistake.
#[test]
fn q_steps_back_and_only_shift_q_quits() {
    use crossterm::event::KeyCode;
    let mut app = app_with(vec![recent(SourceId::Omp, "a", "/w", 1)]);
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.focus(), Pane::Transcript);

    press(&mut app, KeyCode::Char('q'));
    assert_eq!(app.focus(), Pane::Sessions);
    press(&mut app, KeyCode::Char('q'));
    assert_eq!(app.focus(), Pane::Groups);
    press(&mut app, KeyCode::Char('q'));
    assert!(!app.should_quit(), "the leftmost pane is not an exit");
    assert!(app.status().is_some_and(|hint| hint.contains('Q')));

    press(&mut app, KeyCode::Char('Q'));
    assert!(app.should_quit());
}

#[test]
fn focus_cycles_wrap_but_arrows_stop_at_the_edges() {
    use crossterm::event::KeyCode;
    let mut app = app_with(vec![recent(SourceId::Omp, "a", "/w", 1)]);
    for expected in [Pane::Sessions, Pane::Transcript, Pane::Groups] {
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.focus(), expected);
    }
    press(&mut app, KeyCode::Left);
    assert_eq!(app.focus(), Pane::Groups);
    for expected in [Pane::Transcript, Pane::Sessions, Pane::Groups] {
        press(&mut app, KeyCode::BackTab);
        assert_eq!(app.focus(), expected);
    }
    press(&mut app, KeyCode::BackTab);
    press(&mut app, KeyCode::Right);
    assert_eq!(app.focus(), Pane::Transcript);
    press(&mut app, KeyCode::Char(']'));
    assert_eq!(app.grouping(), Grouping::Agent);
}

#[test]
fn help_scrolls_with_bounds_without_navigating_the_underlying_view() {
    use crossterm::event::KeyCode;
    let mut app = app_with(vec![recent(SourceId::Omp, "a", "/w", 1)]);
    app.set_help_viewport(4, 15);
    press(&mut app, KeyCode::Char('?'));
    press(&mut app, KeyCode::PageUp);
    assert_eq!(app.help_scroll(), 0);
    press(&mut app, KeyCode::PageDown);
    assert_eq!(app.help_scroll(), 4);
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.help_scroll(), 3);
    press(&mut app, KeyCode::Char('G'));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.help_scroll(), 11);
    app.set_help_viewport(10, 15);
    assert_eq!(app.help_scroll(), 5);
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.focus(), Pane::Groups);
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('?'));
    assert_eq!(app.help_scroll(), 0);
    press(&mut app, KeyCode::Char('G'));
    press(&mut app, KeyCode::Char('g'));
    assert_eq!(app.help_scroll(), 0);
}

#[test]
fn reflowed_paragraph_can_scroll_to_its_last_word() {
    let mut app = app_with(vec![recent(SourceId::Omp, "a", "/w", 1)]);
    let mut transcript = Transcript::new("a", SourceId::Omp);
    let mut turn = Turn::new(Role::Assistant);
    turn.blocks
        .push(Block::text("alpha beta gamma delta epsilon zeta finalword"));
    transcript.push(turn);
    app.transcript = Some(transcript);
    app.focus = Pane::Transcript;
    app.set_viewport(2);
    // Ten cells of body text behind the two-cell gutter.
    app.set_transcript_width(12);
    app.handle(Action::Bottom);
    assert!(
        app.rendered()[app.scroll()..app.scroll() + 2]
            .iter()
            .any(|line| line.text.contains("finalword"))
    );
    app.set_transcript_width(80);
    assert_eq!(app.scroll(), 0);
    assert!(app.rendered().iter().any(|line| {
        line.text
            .contains("alpha beta gamma delta epsilon zeta finalword")
    }));
}

#[test]
fn rescan_restores_the_selected_transcript_after_rows_reorder() {
    let dir = tempfile::tempdir().unwrap();
    let bucket = dir.path().join(".omp/agent/sessions/bucket");
    std::fs::create_dir_all(&bucket).unwrap();
    let write_session = |id: &str, secs: u64| {
        let path = bucket.join(format!("2026-01-01T00-00-00-000Z_{id}.jsonl"));
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"session\",\"id\":\"{id}\",\"cwd\":\"{}\"}}\n{{\"type\":\"message\",\"message\":{{\"role\":\"user\",\"content\":\"body-{id}\"}}}}\n",
                dir.path().display()
            ),
        ).unwrap();
        std::fs::File::open(path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(secs)),
            )
            .unwrap();
    };
    write_session("older", 100);
    write_session("newer", 200);
    let sources: Vec<Box<dyn Source>> = vec![Box::new(crate::source::omp::Omp::new(dir.path()))];
    let Scan { sessions, failures } = scan_with(&sources, None, None, &[]);
    let mut app = App::new(
        dir.path(),
        sources,
        sessions,
        failures,
        None,
        None,
        Fleet::default(),
    );
    app.handle(Action::FocusNext);
    app.handle(Action::Bottom);
    assert_eq!(app.selected_session().unwrap().id, "older");
    assert!(app.rendered().iter().any(|line| line.text == "body-older"));
    write_session("older", 300);
    app.handle(Action::Refresh);
    assert_eq!(app.selected_session().unwrap().id, "older");
    assert_eq!(app.session_selected(), 0);
    assert_eq!(app.transcript().unwrap().id, "older");
    assert!(app.rendered().iter().any(|line| line.text == "body-older"));
    assert!(!app.rendered().iter().any(|line| line.text == "body-newer"));
}

#[test]
fn workspace_grouping_merges_agents() {
    let app = app_with(vec![
        recent(SourceId::Omp, "a", "/w/one", 1),
        recent(SourceId::Claude, "b", "/w/one", 0),
        recent(SourceId::Codex, "c", "/w/two", 2),
    ]);
    assert_eq!(app.groups()[0].label, "/w/one");
    assert_eq!(app.groups()[0].count, 2);
    assert_eq!(app.groups()[0].agents.len(), 2);
}

#[test]
fn a_running_workspace_outranks_a_bigger_stale_one() {
    let mut busy = recent(SourceId::Omp, "live", "/w/small", 3);
    busy.liveness = Liveness::Live { pid: 1 };
    let mut sessions = vec![busy];
    for i in 0..40 {
        sessions.push(recent(SourceId::Omp, &format!("bulk{i}"), "/w/big", 2));
    }
    let app = app_with(sessions);
    assert_eq!(app.groups()[0].label, "/w/small");
    assert_eq!(app.groups()[0].running, 1);
}

#[test]
fn recency_beats_raw_count() {
    // 40 sessions from six months ago against three from today.
    let mut sessions: Vec<_> = (0..40)
        .map(|i| recent(SourceId::Omp, &format!("old{i}"), "/w/archive", 180))
        .collect();
    sessions.extend((0..3).map(|i| recent(SourceId::Omp, &format!("new{i}"), "/w/active", 0)));
    let app = app_with(sessions);
    assert_eq!(app.groups()[0].label, "/w/active");
}

#[test]
fn children_sit_under_their_parent() {
    let mut child = recent(SourceId::Omp, "kid", "/w", 1);
    child.parent = Some("root".into());
    let mut grandchild = recent(SourceId::Omp, "grandkid", "/w", 1);
    grandchild.parent = Some("kid".into());
    let app = app_with(vec![
        grandchild,
        recent(SourceId::Omp, "root", "/w", 2),
        child,
    ]);
    let depths: Vec<(String, u8)> = app
        .rows()
        .iter()
        .map(|row| (app.session(*row).id.clone(), row.depth))
        .collect();
    assert_eq!(
        depths,
        vec![
            ("root".to_string(), 0),
            ("kid".to_string(), 1),
            ("grandkid".to_string(), 2),
        ]
    );
}

#[test]
fn a_parent_cycle_still_lists_every_row() {
    let mut a = recent(SourceId::Omp, "a", "/w", 1);
    a.parent = Some("b".into());
    let mut b = recent(SourceId::Omp, "b", "/w", 1);
    b.parent = Some("a".into());
    let app = app_with(vec![a, b]);
    assert_eq!(app.visible_len(), 2);
}

#[test]
fn a_parent_outside_the_filter_does_not_hide_its_child() {
    let mut child = recent(SourceId::Omp, "kid", "/w/here", 1);
    child.parent = Some("elsewhere".into());
    let app = app_with(vec![
        child,
        recent(SourceId::Omp, "elsewhere", "/w/other", 1),
    ]);
    // Selected group is one workspace; the child must still appear at depth 0.
    assert_eq!(app.visible_len(), 1);
    assert_eq!(app.rows()[0].depth, 0);
}

fn tree(app: &App) -> Vec<(String, u8)> {
    app.rows()
        .iter()
        .map(|row| (app.session(*row).id.clone(), row.depth))
        .collect()
}

/// `root` → `kid` → `grandkid`, with the session pane focused, which is
/// the only pane fold commands act in.
fn forked_app() -> App {
    let mut child = recent(SourceId::Omp, "kid", "/w", 1);
    child.parent = Some("root".into());
    let mut grandchild = recent(SourceId::Omp, "grandkid", "/w", 1);
    grandchild.parent = Some("kid".into());
    let mut app = app_with(vec![
        grandchild,
        recent(SourceId::Omp, "root", "/w", 2),
        child,
    ]);
    app.handle(Action::FocusNext);
    assert_eq!(app.focus(), Pane::Sessions);
    app
}

#[test]
fn folding_hides_the_whole_branch_and_unfolding_restores_it() {
    let expanded = vec![
        ("root".to_string(), 0),
        ("kid".to_string(), 1),
        ("grandkid".to_string(), 2),
    ];
    let mut app = forked_app();
    assert_eq!(tree(&app), expanded);
    assert!(app.rows()[0].children);
    assert!(!app.rows()[0].folded);

    press(&mut app, crossterm::event::KeyCode::Char('z'));
    press(&mut app, crossterm::event::KeyCode::Char('a'));
    // The folded row stays, and it says so; the branch under it is gone,
    // grandchild included.
    assert_eq!(tree(&app), vec![("root".to_string(), 0)]);
    assert!(app.rows()[0].folded);
    assert_eq!(app.selected_session().unwrap().id, "root");

    press(&mut app, crossterm::event::KeyCode::Char('z'));
    press(&mut app, crossterm::event::KeyCode::Char('a'));
    assert_eq!(tree(&app), expanded);
    assert!(!app.rows()[0].folded);
}

#[test]
fn folding_a_leaf_or_another_pane_changes_no_rows() {
    let mut app = forked_app();
    // `grandkid` is the leaf of the chain.
    app.handle(Action::Down);
    app.handle(Action::Down);
    assert_eq!(app.selected_session().unwrap().id, "grandkid");
    app.handle(Action::ToggleFold);
    assert_eq!(app.visible_len(), 3);
    assert!(app.status().is_some());

    app.handle(Action::FocusNext);
    assert_eq!(app.focus(), Pane::Transcript);
    app.handle(Action::ToggleFold);
    assert_eq!(app.visible_len(), 3);
    app.handle(Action::FocusPrev);
    app.handle(Action::FocusPrev);
    assert_eq!(app.focus(), Pane::Groups);
    app.handle(Action::ToggleFold);
    assert_eq!(app.visible_len(), 3);
}

#[test]
fn a_fold_outlives_a_regroup() {
    let mut app = forked_app();
    app.handle(Action::CloseFold);
    assert_eq!(app.visible_len(), 1);
    // Showing archived sessions rebuilds groups and rows from scratch.
    app.handle(Action::ToggleArchived);
    assert_eq!(tree(&app), vec![("root".to_string(), 0)]);
    app.handle(Action::ToggleArchived);
    assert_eq!(tree(&app), vec![("root".to_string(), 0)]);
}

/// A fold belongs to the session that owns the branch. Where that
/// session is not in the view, its child is a root of its own and must
/// not inherit the fold.
#[test]
fn a_fold_on_an_absent_parent_does_not_hide_the_promoted_child() {
    let mut child = recent(SourceId::Omp, "kid", "/w/here", 0);
    child.parent = Some("elsewhere".into());
    let mut app = app_with(vec![
        child,
        recent(SourceId::Omp, "elsewhere", "/w/other", 1),
    ]);
    cycle_to(&mut app, Grouping::Timeline);
    app.handle(Action::FocusNext);
    assert_eq!(app.selected_session().unwrap().id, "elsewhere");
    app.handle(Action::CloseFold);
    assert_eq!(tree(&app), vec![("elsewhere".to_string(), 0)]);

    // Back in workspace view the parent sits in another group, so the
    // child is its own root: visible, flat, and with nothing to unfold.
    cycle_to(&mut app, Grouping::Workspace);
    assert_eq!(tree(&app), vec![("kid".to_string(), 0)]);
    assert!(!app.rows()[0].children);
    assert!(!app.rows()[0].folded);
}

#[test]
fn close_all_then_open_selected_keeps_other_branches_closed() {
    let mut left_child = recent(SourceId::Omp, "left-child", "/w", 1);
    left_child.parent = Some("left".into());
    let mut right_child = recent(SourceId::Omp, "right-child", "/w", 3);
    right_child.parent = Some("right".into());
    let mut app = app_with(vec![
        left_child,
        recent(SourceId::Omp, "left", "/w", 0),
        right_child,
        recent(SourceId::Omp, "right", "/w", 2),
    ]);
    app.handle(Action::FocusNext);

    press(&mut app, crossterm::event::KeyCode::Char('z'));
    press(&mut app, crossterm::event::KeyCode::Char('M'));
    assert_eq!(
        tree(&app),
        vec![("left".to_string(), 0), ("right".to_string(), 0)]
    );

    press(&mut app, crossterm::event::KeyCode::Char('z'));
    press(&mut app, crossterm::event::KeyCode::Char('o'));
    assert_eq!(
        tree(&app),
        vec![
            ("left".to_string(), 0),
            ("left-child".to_string(), 1),
            ("right".to_string(), 0),
        ]
    );
    assert!(app.rows()[2].folded, "the unselected branch stays closed");
}

#[test]
fn archived_sessions_are_hidden_until_asked_for() {
    let mut archived = recent(SourceId::Omp, "z", "/w/one", 1);
    archived.archived = true;
    let mut app = app_with(vec![archived, recent(SourceId::Omp, "a", "/w/one", 2)]);
    assert_eq!(app.visible_len(), 1);
    app.handle(Action::ToggleArchived);
    assert_eq!(app.visible_len(), 2);
}

/// Cycle until the wanted mode; the order is an implementation detail
/// that this helper keeps out of every other test.
fn cycle_to(app: &mut App, wanted: Grouping) {
    for _ in 0..6 {
        if app.grouping() == wanted {
            return;
        }
        app.handle(Action::CycleGroupingNext);
    }
    panic!("{wanted:?} unreachable by cycling");
}

/// Cycling is four presses away from the view you want; a digit is one.
#[test]
fn a_digit_jumps_straight_to_a_view() {
    use crossterm::event::KeyCode;
    let mut app = app_with(vec![
        recent(SourceId::Omp, "a", "/w/one", 1),
        recent(SourceId::Codex, "c", "/w/two", 2),
    ]);
    for (digit, wanted) in [
        ('6', Grouping::Timeline),
        ('5', Grouping::Host),
        ('2', Grouping::Agent),
        ('4', Grouping::Fleet),
        ('3', Grouping::Branch),
        ('1', Grouping::Workspace),
    ] {
        press(&mut app, KeyCode::Char(digit));
        assert_eq!(app.grouping(), wanted, "{digit} selects {wanted:?}");
        assert_eq!(app.group_selected(), 0, "the axis changed, so does row 1");
    }
    // A digit is literal text while a prompt is open.
    press(&mut app, KeyCode::Char('/'));
    press(&mut app, KeyCode::Char('3'));
    assert_eq!(app.prompt(), &Prompt::Filter("3".into()));
    assert_eq!(app.grouping(), Grouping::Workspace);
}

#[test]
fn timeline_grouping_collapses_to_one_bucket() {
    let mut app = app_with(vec![
        recent(SourceId::Omp, "a", "/w/one", 1),
        recent(SourceId::Codex, "c", "/w/two", 2),
    ]);
    cycle_to(&mut app, Grouping::Agent);
    assert_eq!(app.groups().len(), 2);
    cycle_to(&mut app, Grouping::Timeline);
    assert_eq!(app.groups().len(), 1);
    assert_eq!(app.visible_len(), 2);
}

/// Two machines with the same directory are two projects: the host
/// qualifies the key, and view 5 names the machine beside every bucket.
/// A bookmark must survive being left: the letter puts the view, the row
/// and the focus back exactly as they were saved, including the file.
#[test]
fn a_bookmark_restores_view_selection_and_focus() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with(vec![
        recent(SourceId::Omp, "one", "/w/one", 3),
        recent(SourceId::Codex, "two", "/w/two", 2),
    ]);
    app.bookmark_path = dir.path().join("aliases.json");

    // Groups sort by recency, so the newer session's workspace is row 0;
    // `Top` lands on it deterministically. Focus the reader afterwards:
    // `Down` under Transcript focus scrolls the reader, not the rows.
    app.handle(Action::Top);
    app.focus = Pane::Transcript;
    app.handle(Action::StartBookmark);
    app.handle(Action::Char('a'));
    assert!(
        app.status.as_deref().is_some_and(|s| s.contains("saved")),
        "{:?}",
        app.status
    );

    // Wander: another view, another row, another pane.
    app.handle(Action::SetGrouping(Grouping::Agent));
    app.handle(Action::Up);
    app.focus = Pane::Groups;

    app.handle(Action::StartJump);
    app.handle(Action::Char('a'));
    assert_eq!(app.grouping(), Grouping::Workspace, "the view is restored");
    assert_eq!(
        app.selected_session().map(|s| s.id.as_str()),
        Some("two"),
        "the row is restored"
    );
    assert_eq!(app.focus, Pane::Transcript, "the pane is restored");

    // The file outlives the process.
    let (store, failure) = crate::storage::alias::Store::load_at(&app.bookmark_path);
    assert_eq!(failure, None);
    let saved = store.get('a').expect("persisted");
    assert_eq!(saved.id, "two");
    assert_eq!(saved.source, SourceId::Codex);
    assert_eq!(saved.host, None);
    assert_eq!(saved.view, "workspace");
    assert_eq!(saved.focus, 't');
}

#[test]
fn a_bookmark_for_a_missing_letter_or_session_says_so() {
    let mut app = app_with(vec![recent(SourceId::Omp, "one", "/w/one", 1)]);
    app.handle(Action::StartJump);
    app.handle(Action::Char('z'));
    assert_eq!(app.status.as_deref(), Some("no bookmark 'z"));

    app.bookmarks.marks.insert(
        'g',
        crate::storage::alias::Bookmark {
            host: None,
            source: SourceId::Omp,
            id: "vanished".into(),
            view: "timeline".into(),
            focus: 's',
        },
    );
    app.handle(Action::StartJump);
    app.handle(Action::Char('g'));
    assert!(
        app.status.as_deref().is_some_and(|s| s.contains("gone")),
        "{:?}",
        app.status
    );
}

#[test]
fn read_only_refuses_to_save_a_bookmark() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with(vec![recent(SourceId::Omp, "one", "/w/one", 1)]);
    app.bookmark_path = dir.path().join("aliases.json");
    app.set_read_only(true);
    app.handle(Action::StartBookmark);
    app.handle(Action::Char('a'));
    assert!(
        app.status
            .as_deref()
            .is_some_and(|s| s.contains("read-only")),
        "{:?}",
        app.status
    );
    assert!(app.bookmarks.marks.is_empty());
}

#[test]
fn the_host_view_keeps_two_machines_apart() {
    let mut far = recent(SourceId::Omp, "far", "/var/lib/hermes/work", 1);
    far.host = Some("nx-1".into());
    let mut app = app_with(vec![
        recent(SourceId::Omp, "near", "/var/lib/hermes/work", 2),
        far,
    ]);

    // Workspace view: two groups, not one merged project.
    assert_eq!(app.groups().len(), 2, "same path, different machines");

    app.handle(Action::SetGrouping(Grouping::Host));
    let mut labels: Vec<String> = app
        .groups()
        .iter()
        .map(|group| group.label.clone())
        .collect();
    labels.sort();
    assert_eq!(labels, vec!["local".to_string(), "nx-1".to_string()]);
}

#[test]
fn branch_grouping_splits_one_repository_by_branch() {
    let mut one = recent(SourceId::Omp, "a", "/w/repo", 1);
    one.branch = Some("feat/one".into());
    let mut two = recent(SourceId::Omp, "b", "/w/repo", 2);
    two.branch = Some("feat/two".into());
    // A worktree stack rolls up into one repository row, which is right,
    // and then hides the only thing telling the sessions apart.
    let mut app = app_with(vec![one, two]);
    assert_eq!(app.groups().len(), 1);
    cycle_to(&mut app, Grouping::Branch);
    assert_eq!(app.groups().len(), 2);
    assert!(app.groups().iter().any(|g| g.label.ends_with("@ feat/one")));
}

#[test]
fn fleet_grouping_shows_only_attached_processes() {
    let mut running = recent(SourceId::Omp, "live", "/w/one", 1);
    running.liveness = Liveness::Live { pid: 42 };
    let mut app = app_with(vec![running, recent(SourceId::Omp, "idle", "/w/one", 2)]);
    cycle_to(&mut app, Grouping::Fleet);
    assert_eq!(app.groups().len(), 1);
    assert_eq!(app.visible_len(), 1);
    assert_eq!(app.selected_session().unwrap().id, "live");
}

#[test]
fn filter_matches_title_workspace_and_agent() {
    let mut app = app_with(vec![
        recent(SourceId::Omp, "alpha", "/w/one", 1),
        recent(SourceId::Codex, "beta", "/w/two", 2),
    ]);
    cycle_to(&mut app, Grouping::Timeline);
    app.handle(Action::StartFilter);
    for c in "source:codex".chars() {
        app.handle(Action::Char(c));
    }
    app.handle(Action::Confirm);
    assert_eq!(app.visible_len(), 1);
    assert_eq!(app.selected_session().unwrap().id, "beta");
}

/// Fleet mode answers "what is this instance doing", and the children it
/// spawned are part of the answer — including the ones that already
/// finished and therefore hold no descriptor of their own.
#[test]
fn fleet_keeps_a_live_session_and_the_subagents_it_spawned() {
    let mut parent = recent(SourceId::Omp, "parent", "/w", 1);
    parent.liveness = Liveness::Live { pid: 42 };
    let mut child = recent(SourceId::Omp, "kid", "/w", 2);
    child.subagent = true;
    child.parent = Some("parent".into());
    // A fork shares an ancestor but is its own conversation.
    let mut fork = recent(SourceId::Omp, "fork", "/w", 3);
    fork.parent = Some("parent".into());
    let mut app = app_with(vec![
        parent,
        child,
        fork,
        recent(SourceId::Omp, "idle", "/w", 4),
    ]);

    cycle_to(&mut app, Grouping::Fleet);
    assert_eq!(app.groups().len(), 1, "one bucket per process");
    let ids: Vec<&str> = app
        .rows()
        .iter()
        .map(|&row| app.session(row).id.as_str())
        .collect();
    assert_eq!(ids, ["parent", "kid"]);
}

#[test]
fn marking_moves_on_and_archive_asks_first() {
    let mut app = app_with(vec![
        recent(SourceId::Omp, "a", "/w/one", 1),
        recent(SourceId::Omp, "b", "/w/one", 2),
    ]);
    app.handle(Action::ToggleMark);
    assert_eq!(app.marked_count(), 1);
    app.handle(Action::ArchiveMarked);
    assert!(matches!(
        app.prompt(),
        Prompt::ConfirmArchive {
            count: 1,
            groups: 0,
            archive: true
        }
    ));
    app.handle(Action::Cancel);
    assert_eq!(app.prompt(), &Prompt::None);
    assert_eq!(app.marked_count(), 1);
}

#[test]
fn marking_a_workspace_targets_every_session_in_it() {
    let mut app = app_with(vec![
        recent(SourceId::Omp, "a", "/w/one", 1),
        recent(SourceId::Claude, "b", "/w/one", 2),
        recent(SourceId::Omp, "c", "/w/two", 3),
    ]);
    // /w/one is the most recent group, so it is already selected.
    app.handle(Action::ToggleGroupMark);
    app.handle(Action::ArchiveMarked);
    assert!(matches!(
        app.prompt(),
        Prompt::ConfirmArchive {
            count: 2,
            groups: 1,
            ..
        }
    ));
    app.handle(Action::Confirm);
    let archived: Vec<&str> = app
        .sessions
        .iter()
        .filter(|s| s.archived)
        .map(|s| s.id.as_str())
        .collect();
    assert_eq!(archived, vec!["a", "b"]);
    // The selection is consumed, so a second `A` cannot re-fire it.
    assert_eq!(app.marked_group_count(), 0);
}

#[test]
fn a_marked_workspace_covers_sessions_the_filter_hides() {
    let mut hidden = recent(SourceId::Omp, "hidden", "/w/one", 1);
    hidden.title = Some("nothing matching".into());
    let mut app = app_with(vec![hidden, recent(SourceId::Omp, "shown", "/w/one", 2)]);
    app.handle(Action::ToggleGroupMark);
    app.handle(Action::StartFilter);
    for c in "shown".chars() {
        app.handle(Action::Char(c));
    }
    app.handle(Action::Confirm);
    assert_eq!(app.visible_len(), 1);
    // Retiring a project must not leave its filtered-out sessions behind.
    app.handle(Action::ArchiveMarked);
    assert!(matches!(
        app.prompt(),
        Prompt::ConfirmArchive { count: 2, .. }
    ));
}

#[test]
fn archiving_an_already_archived_workspace_is_a_no_op_not_a_lie() {
    let mut done = recent(SourceId::Omp, "a", "/w/one", 1);
    done.archived = true;
    let mut app = app_with(vec![done]);
    app.handle(Action::ToggleArchived);
    app.handle(Action::ToggleGroupMark);
    app.handle(Action::ArchiveMarked);
    assert_eq!(app.prompt(), &Prompt::None);
    assert!(
        app.status()
            .is_some_and(|s| s.contains("nothing to archive"))
    );
}

#[test]
fn resizing_keeps_the_three_panes_summing_to_a_hundred() {
    let mut app = app_with(vec![recent(SourceId::Omp, "a", "/w", 1)]);
    app.handle(Action::StartResize);
    for _ in 0..5 {
        app.handle(Action::Widen);
    }
    assert_eq!(app.widths().iter().sum::<u16>(), 100);
    assert!(app.widths()[0] > DEFAULT_WIDTHS[0]);
    app.handle(Action::Char('='));
    assert_eq!(app.widths(), DEFAULT_WIDTHS);
}

#[test]
fn a_pane_cannot_be_squeezed_out_of_existence() {
    let mut app = app_with(vec![recent(SourceId::Omp, "a", "/w", 1)]);
    app.handle(Action::StartResize);
    for _ in 0..200 {
        app.handle(Action::Narrow);
    }
    assert!(app.widths().iter().all(|&w| w >= MIN_WIDTH));
    assert_eq!(app.widths().iter().sum::<u16>(), 100);
}

#[test]
fn resize_mode_is_sticky_until_dismissed() {
    let mut app = app_with(vec![recent(SourceId::Omp, "a", "/w", 1)]);
    app.handle(Action::StartResize);
    app.handle(Action::Widen);
    assert_eq!(app.prompt(), &Prompt::Resize);
    app.handle(Action::Cancel);
    assert_eq!(app.prompt(), &Prompt::None);
    // Outside the mode, arrows navigate again rather than resizing.
    let before = app.widths();
    app.handle(Action::FocusNext);
    assert_eq!(app.widths(), before);
    assert_eq!(app.focus(), Pane::Sessions);
}

#[test]
fn the_last_pane_trades_width_with_its_left_neighbour() {
    let mut app = app_with(vec![recent(SourceId::Omp, "a", "/w", 1)]);
    app.handle(Action::FocusNext);
    app.handle(Action::FocusNext);
    assert_eq!(app.focus(), Pane::Transcript);
    app.handle(Action::StartResize);
    app.handle(Action::Widen);
    assert_eq!(app.widths()[0], DEFAULT_WIDTHS[0], "groups untouched");
    assert!(app.widths()[2] > DEFAULT_WIDTHS[2]);
    assert_eq!(app.widths().iter().sum::<u16>(), 100);
}

#[test]
fn live_only_hides_idle_sessions_and_their_empty_workspaces() {
    let mut running = recent(SourceId::Omp, "live", "/w/one", 1);
    running.liveness = Liveness::Live { pid: 1 };
    let mut app = app_with(vec![
        running,
        recent(SourceId::Omp, "idle", "/w/one", 2),
        recent(SourceId::Omp, "quiet", "/w/two", 3),
    ]);
    assert_eq!(app.groups().len(), 2);

    app.handle(Action::ToggleLiveOnly);
    assert_eq!(
        app.groups().len(),
        1,
        "a workspace with nothing live disappears"
    );
    assert_eq!(app.visible_len(), 1);
    assert_eq!(app.selected_session().unwrap().id, "live");

    app.handle(Action::ToggleLiveOnly);
    assert_eq!(app.visible_len(), 2);
}

#[test]
fn an_inferred_attachment_still_counts_as_live() {
    let mut guessed = recent(SourceId::OpenCode, "maybe", "/w", 1);
    guessed.liveness = Liveness::Probably { pid: 9 };
    let mut recent_only = recent(SourceId::Omp, "warm", "/w", 1);
    recent_only.liveness = Liveness::Recent;
    let mut app = app_with(vec![guessed, recent_only]);
    app.handle(Action::ToggleLiveOnly);
    // `Recent` means nothing is attached, so it is not "live".
    assert_eq!(app.visible_len(), 1);
    assert_eq!(app.selected_session().unwrap().id, "maybe");
}

#[test]
fn clearing_filters_also_clears_live_only() {
    let mut app = app_with(vec![recent(SourceId::Omp, "a", "/w", 1)]);
    app.handle(Action::ToggleLiveOnly);
    assert_eq!(app.visible_len(), 0);
    app.handle(Action::ClearFilter);
    assert!(!app.live_only());
    assert_eq!(app.visible_len(), 1);
}

#[test]
fn only_enter_follows_a_link_including_wrapped_continuations() {
    use crossterm::event::KeyCode;
    let mut child = recent(SourceId::Omp, "child", "/w", 1);
    child.subagent = true;
    child.parent = Some("root".into());
    child.title = Some("SpecReview".into());
    let mut app = app_with(vec![recent(SourceId::Omp, "root", "/w", 2), child]);
    app.set_read_only(true);
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.focus(), Pane::Transcript);
    app.rendered = wrap_rendered(
        vec![
            RenderedLine::new(
                LineKind::Link,
                Rail::Mark("↳ "),
                "open the SpecReview subagent transcript",
            )
            .with_link((SourceId::Omp, "child".into())),
        ],
        12,
    );
    press(&mut app, KeyCode::Char('G'));
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Char('l'));
    assert_eq!(app.selected_session().unwrap().id, "root");
    assert_eq!(app.focus(), Pane::Transcript);
    assert_eq!(app.visible_len(), 2, "the child nests under its parent");

    press(&mut app, KeyCode::Enter);
    assert_eq!(app.selected_session().unwrap().id, "child");
    assert_eq!(app.focus(), Pane::Transcript);
    press(&mut app, KeyCode::Char('J'));
    assert!(!app.should_quit());
    assert_eq!(app.selected_session().unwrap().id, "child");
    assert!(app.status().is_some_and(|s| s.contains("read-only")));

    assert!(!app.jump_to(&(SourceId::Omp, "elsewhere".to_string())));
    assert!(app.status().is_some_and(|s| s.contains("not in this view")));
}

#[test]
fn read_only_refuses_to_even_ask_about_archiving() {
    let mut app = app_with(vec![recent(SourceId::Omp, "a", "/w/one", 1)]);
    app.set_read_only(true);
    app.handle(Action::ArchiveMarked);
    assert_eq!(app.prompt(), &Prompt::None);
    assert!(app.status().is_some_and(|s| s.contains("read-only")));
}

#[test]
fn folding_switches_between_summary_and_body() {
    let mut transcript = Transcript::new("t", SourceId::Omp);
    let mut turn = Turn::new(Role::Assistant);
    turn.blocks.push(Block::thinking("one\ntwo\nthree"));
    transcript.push(turn);

    let folded = render(
        &transcript,
        true,
        true,
        false,
        &HashMap::new(),
        UtcOffset::UTC,
    );
    let summary = folded
        .iter()
        .find(|l| l.kind == LineKind::Thinking)
        .expect("a folded thinking line");
    assert_eq!(summary.text, "thinking");
    assert_eq!(summary.meta.as_deref(), Some("3 lines"));
    let open = render(
        &transcript,
        false,
        true,
        false,
        &HashMap::new(),
        UtcOffset::UTC,
    );
    assert!(open.iter().any(|l| l.text == "two" && l.rail == Rail::Pipe));
}

/// Fullscreen draws no status bar and no list, so anything modal or
/// list-shaped must not be reachable from behind it: an invisible prompt
/// swallows every key, including the one that leaves.
#[test]
fn fullscreen_never_traps_input_behind_invisible_chrome() {
    use crossterm::event::KeyCode;
    let mut app = app_with(vec![recent(SourceId::Omp, "a", "/w", 1)]);

    // Help is modal but is drawn over fullscreen, so it stays open there.
    press(&mut app, KeyCode::F(11));
    press(&mut app, KeyCode::Char('?'));
    assert!(app.show_help() && app.zen());
    press(&mut app, KeyCode::Esc);
    assert!(!app.show_help(), "Esc closes help, not fullscreen");
    assert!(app.zen(), "closing help leaves the reader where it was");
    press(&mut app, KeyCode::F(11));
    assert!(!app.zen());

    // A prompt needs the status bar, so it drops fullscreen first.
    for (key, opened) in [
        (KeyCode::Char('/'), Prompt::Filter(String::new())),
        (KeyCode::Char('s'), Prompt::Search(String::new())),
    ] {
        press(&mut app, KeyCode::F(11));
        press(&mut app, key);
        assert!(!app.zen(), "{key:?} left fullscreen");
        assert_eq!(app.prompt(), &opened);
        press(&mut app, KeyCode::Esc);
    }
}

#[test]
fn fullscreen_opens_the_reader_and_escape_returns_to_the_panes() {
    use crossterm::event::KeyCode;

    let mut app = app_with(vec![recent(SourceId::Omp, "a", "/w", 1)]);
    app.handle(Action::StartResize);
    press(&mut app, KeyCode::Char('o'));
    assert!(app.zen());
    assert_eq!(app.focus(), Pane::Transcript);
    // Reading keys keep working; pane navigation leaves the mode.
    press(&mut app, KeyCode::Char('t'));
    assert!(app.zen());
    press(&mut app, KeyCode::Esc);
    assert!(!app.zen());
    assert_eq!(
        app.focus(),
        Pane::Transcript,
        "leaving fullscreen is not going back a pane"
    );
    app.handle(Action::StartResize);
    press(&mut app, KeyCode::Char('o'));
    app.handle(Action::StartResize);
    press(&mut app, KeyCode::Char('o'));
    assert!(!app.zen());
}
// ---- drawing, through ratatui's TestBackend ----

fn paint(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| {
            let layout = crate::tui::ui::panes(frame.area(), app);
            app.apply_layout(&layout);
            crate::tui::ui::draw(frame, app, &layout);
        })
        .expect("draw");
    terminal.backend().buffer().clone()
}

fn painted_text(buffer: &ratatui::buffer::Buffer) -> String {
    buffer
        .content
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect()
}

#[test]
fn transcript_bottom_reaches_the_visible_last_row_in_panes_and_zen() {
    let mut app = app_with(vec![recent(SourceId::Omp, "a", "/w", 1)]);
    let mut transcript = Transcript::new("a", SourceId::Omp);
    let mut turn = Turn::new(Role::Assistant);
    turn.blocks.push(Block::text(format!(
        "```\n{}\n```",
        (0..60)
            .map(|n| format!("line-{n:02}"))
            .chain(std::iter::once("finalword".to_owned()))
            .collect::<Vec<_>>()
            .join("\n"),
    )));
    transcript.push(turn);
    app.focus = Pane::Transcript;
    app.transcript = Some(transcript);
    app.rerender();

    for (zen, height, expected_rows, last_row) in [
        (false, 18, 14, 15),
        (true, 18, 9, 13),
        (false, 5, 1, 2),
        (true, 10, 1, 5),
    ] {
        app.zen = zen;
        paint(&mut app, 80, height);
        assert_eq!(app.viewport, expected_rows, "zen={zen}, height={height}");
        app.handle(Action::Bottom);
        let buffer = paint(&mut app, 80, height);
        let visible = (0..80)
            .map(|x| buffer[(x, last_row)].symbol())
            .collect::<String>();
        assert!(
            visible.contains("finalword"),
            "last transcript line not visible on its final body row: zen={zen}, height={height}, row={visible:?}"
        );
        assert_eq!(app.scroll + app.viewport, app.rendered().len());
    }
}

#[test]
fn too_small_to_show_a_transcript_reports_zero_rows_then_recovers() {
    let mut app = app_with(vec![recent(SourceId::Omp, "a", "/w", 1)]);
    app.focus = Pane::Transcript;
    for zen in [false, true] {
        app.zen = zen;
        for (width, height) in [(3, 0), (3, 1), (3, 2), (3, 3), (3, 20), (80, 3)] {
            let layout =
                crate::tui::ui::panes(ratatui::layout::Rect::new(0, 0, width, height), &app);
            app.apply_layout(&layout);
            assert_eq!(layout.viewport, 0, "zen={zen}, size={width}x{height}");
            assert_eq!(app.viewport, 0, "zen={zen}, size={width}x{height}");
            if width == 3 {
                assert_eq!(layout.transcript_width, 0);
            }
        }
        let layout = crate::tui::ui::panes(ratatui::layout::Rect::new(0, 0, 80, 20), &app);
        app.apply_layout(&layout);
        assert!(app.viewport > 0);
        assert!(layout.transcript_width > 0);
    }
}

#[test]
fn drawing_is_pure_and_the_filter_prompt_reaches_the_status_bar() {
    let mut app = app_with(vec![recent(SourceId::Omp, "a", "/w", 1)]);
    app.handle(Action::StartFilter);
    app.handle(Action::Char('g'));
    let first = paint(&mut app, 100, 24);
    assert!(
        painted_text(&first).contains("Filter: g_"),
        "the status bar must show the filter being typed"
    );
    let second = paint(&mut app, 100, 24);
    assert_eq!(first, second, "drawing twice must paint the same frame");
}

#[test]
fn zen_hides_the_chrome_and_help_opens_over_it() {
    let mut app = app_with(vec![recent(SourceId::Omp, "a", "/w", 1)]);
    app.handle(Action::Confirm);
    app.handle(Action::ToggleZen);
    let zen = paint(&mut app, 100, 30);
    assert!(
        !painted_text(&zen).contains("llm-sessions"),
        "zen mode draws no breadcrumb"
    );
    app.handle(Action::ToggleHelp);
    let helped = paint(&mut app, 100, 30);
    assert!(
        painted_text(&helped).contains("Help"),
        "help must render over zen"
    );
}

/// A remote row's locator names a path on the *host*. Reading it through
/// a local adapter opens whatever local file shares that name, or fails
/// claiming the host's own store is unreadable — which is what a fleet
/// with a database-backed agent saw.
#[test]
fn a_remote_row_is_read_through_its_host_not_the_local_filesystem() {
    let home = Path::new("/home/u");
    let mut row = recent(SourceId::Hermes, "abc123", "/srv/work", 1);
    row.host = Some("nx-1".into());
    row.locator = Locator::Row {
        db: PathBuf::from("/home/arch/.hermes/state.db"),
        id: "abc123".into(),
    };
    let app = App::new(
        home,
        crate::source::all(home),
        vec![row],
        Vec::new(),
        None,
        None,
        Fleet::default(),
    );

    // The selected row's body is loaded eagerly, as for a local one.
    let status = app.status().unwrap_or_default().to_string();
    assert!(
        !status.contains("unable to open database"),
        "the local store was opened for a remote row: {status}"
    );
    // The TUI's cache has no hosts in tests, so routing stops there —
    // naming the host, never the local path.
    assert!(status.contains("nx-1"), "{status}");
    assert!(app.transcript().is_none());
}
