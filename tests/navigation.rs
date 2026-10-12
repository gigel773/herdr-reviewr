//! Code navigation through the pane: `open_at`, the locations list, and `R`'s choice.

mod common;

use common::{Repo, app_on, enter_tab, land_world};
use herdr_reviewr::app::{App, Mode, Tab};
use herdr_reviewr::locations::Stage;
use herdr_reviewr::lsp::uri::Located;
use herdr_reviewr::lsp::{Context, Kind, Location, LspCompletion, Outcome, Query};

fn repo() -> Repo {
    let r = Repo::init();
    r.write("src/main.cpp", "int main() {\n  auto user = loadUser(id);\n}\n");
    r.write("src/user.cpp", "User loadUser(int id) {\n  return User{};\n}\n");
    r.write("README.md", "# x\n");
    r.commit_all("init");
    r
}

/// The pane in `All files` on `path`, cursor on `line`.
fn on(r: &Repo, path: &str, line: u32) -> App {
    let mut app = app_on(r);
    enter_tab(&mut app, Tab::AllFiles);
    assert!(app.open_at(path, Some(line)).unwrap());
    app
}

fn at(path: &str, line: u32, text: &str) -> Location {
    Location {
        place: Located::Repo(path.into()),
        line,
        text: text.into(),
        context: Context::default(),
    }
}

/// The query the pane would dispatch, taken as the event loop takes it.
fn query(app: &mut App) -> Query {
    app.lsp_query.take().unwrap_or_else(|| panic!("a query; status: {}", app.status))
}

fn answer(app: &mut App, q: &Query, occurrence: usize, found: Vec<Location>) {
    let outcome = Outcome::Ready(found);
    app.apply_lsp_completion(LspCompletion {
        tag: q.tag.clone(),
        occurrence: Some(occurrence),
        outcome,
    });
}

fn place(app: &App) -> (Tab, Option<String>, usize) {
    (app.tab, app.diff_path.clone(), app.diff_cursor)
}

#[test]
fn open_at_lands_on_the_line_and_a_vanished_file_moves_nothing() {
    let r = repo();
    let mut app = on(&r, "src/user.cpp", 2);
    assert_eq!(place(&app), (Tab::AllFiles, Some("src/user.cpp".into()), 1));
    let before = place(&app);
    assert!(!app.open_at("src/gone.cpp", Some(1)).unwrap());
    assert_eq!(place(&app), before);
}

#[test]
fn a_file_newer_than_the_listing_is_listed_and_picked_in_the_same_press() {
    let r = repo();
    let mut app = on(&r, "src/main.cpp", 1);
    r.write("src/new.cpp", "int fresh;\n");
    assert!(app.open_at("src/new.cpp", Some(1)).unwrap());
    // The frame after the press: the navigator and the read pane name one file.
    let row = app.file_rows[app.file_cursor].file_index().map(|i| app.entries[i].path.clone());
    assert_eq!(row.as_deref(), Some("src/new.cpp"));
    assert_eq!(app.diff_path.as_deref(), Some("src/new.cpp"));
    land_world(&mut app);
    let row = app.file_rows[app.file_cursor].file_index().map(|i| app.entries[i].path.clone());
    assert_eq!(row.as_deref(), Some("src/new.cpp"), "the real row replaces the provisional one");
}

#[test]
fn a_reviewer_who_moved_on_before_the_refresh_keeps_their_pick() {
    let r = repo();
    let mut app = on(&r, "src/main.cpp", 1);
    r.write("src/new.cpp", "int fresh;\n");
    app.open_at("src/new.cpp", Some(1)).unwrap();
    let readme = app
        .file_rows
        .iter()
        .position(|row| row.file_index().is_some_and(|i| app.entries[i].path == "README.md"));
    app.select_file(readme.unwrap()).unwrap();
    land_world(&mut app);
    assert_eq!(app.diff_path.as_deref(), Some("README.md"));
}

#[test]
fn the_list_opens_at_once_and_answers_fill_it_without_moving_the_pick() {
    let r = repo();
    let mut app = on(&r, "src/main.cpp", 2);
    let before = place(&app);
    app.ask_lsp(Kind::Definition).unwrap();
    assert_eq!(app.mode, Mode::Locations, "open before any answer");
    let q = query(&mut app);
    let names: Vec<&str> = q.occurrences.iter().map(|(_, o)| o.name.as_str()).collect();
    assert_eq!(names, ["auto", "user", "loadUser", "id"]);
    assert_eq!((q.kind, q.tag.path.as_str(), q.tag.line), (Kind::Definition, "src/main.cpp", 2));
    // `id` answers first and takes the pick; `loadUser` landing above leaves it there.
    answer(&mut app, &q, 3, vec![at("src/user.cpp", 1, "User loadUser(int id) {")]);
    let picked = app.locations.as_ref().unwrap().picked_place().cloned();
    answer(&mut app, &q, 2, vec![at("src/user.cpp", 1, "User loadUser(int id) {")]);
    answer(&mut app, &q, 0, Vec::new());
    answer(&mut app, &q, 1, vec![at("src/main.cpp", 2, "auto user = loadUser(id);")]);
    assert_eq!(app.locations.as_ref().unwrap().picked_place().cloned(), picked);
    assert_eq!(place(&app), before, "answers never move the place");
    // `esc` leaves everything as it was.
    app.close_locations();
    assert_eq!((app.mode.clone(), place(&app)), (Mode::Normal, before));
}

#[test]
fn enter_opens_a_worktree_place_and_not_one_outside() {
    let r = repo();
    let mut app = on(&r, "src/main.cpp", 2);
    app.ask_lsp(Kind::Definition).unwrap();
    let q = query(&mut app);
    let outside = Location {
        place: Located::External("/usr/include/x.h".into()),
        line: 7,
        text: "int x;".into(),
        context: Context::default(),
    };
    answer(&mut app, &q, 2, vec![outside]);
    app.locations_open().unwrap();
    assert_eq!(app.mode, Mode::Locations, "an outside place opens only in the editor");
    assert!(app.status.contains("outside the worktree"), "{}", app.status);
    answer(&mut app, &q, 3, vec![at("src/user.cpp", 2, "return User{};")]);
    app.locations_move(1);
    app.locations_open().unwrap();
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(place(&app), (Tab::AllFiles, Some("src/user.cpp".into()), 1));
    assert!(app.reveal_lead, "the jump leads with the definition, its body below");
}

#[test]
fn edit_opens_any_picked_place_at_its_line_outside_the_worktree_too() {
    let r = repo();
    let mut app = on(&r, "src/main.cpp", 2);
    app.ask_lsp(Kind::Definition).unwrap();
    let q = query(&mut app);
    let header = std::path::PathBuf::from("/usr/include/c++/13/bits/shared_ptr.h");
    let outside = Location {
        place: Located::External(header.clone()),
        line: 88,
        text: "class shared_ptr".into(),
        context: Context::default(),
    };
    answer(&mut app, &q, 2, vec![outside]);
    app.locations_edit();
    let target = app.editor_request.take().expect("an editor request");
    assert_eq!((target.path, target.line), (header, 88));
    assert_eq!(app.mode, Mode::Locations, "the list stays for the next pick");
    answer(&mut app, &q, 3, vec![at("src/user.cpp", 1, "User loadUser(int id) {")]);
    app.locations_move(1);
    app.locations_edit();
    let target = app.editor_request.take().expect("an editor request");
    assert_eq!((target.path, target.line), ("src/user.cpp".into(), 1));
}

#[test]
fn an_answer_for_another_query_never_lands() {
    let r = repo();
    let mut app = on(&r, "src/main.cpp", 2);
    app.ask_lsp(Kind::Definition).unwrap();
    let old = query(&mut app);
    app.close_locations();
    assert!(app.lsp_cancel, "the query still out is cancelled");
    app.ask_lsp(Kind::Definition).unwrap();
    let new = query(&mut app);
    answer(&mut app, &old, 2, vec![at("src/user.cpp", 1, "stale")]);
    assert!(
        app.locations
            .as_ref()
            .unwrap()
            .rows()
            .iter()
            .all(|row| !matches!(row, herdr_reviewr::locations::Row::Place { .. }))
    );
    answer(&mut app, &new, 2, vec![at("src/user.cpp", 1, "fresh")]);
    assert_eq!(
        app.locations.as_ref().unwrap().picked_place().map(|p| p.text.as_str()),
        Some("fresh")
    );
}

#[test]
fn references_skip_the_choice_for_one_symbol_and_one_definition() {
    let r = repo();
    r.write("src/one.cpp", "  run();\n  foo(1) + foo(2);\n  foo(1) + foo(\"a\");\n");
    let mut app = on(&r, "src/one.cpp", 1);
    app.ask_lsp(Kind::References).unwrap();
    let q = query(&mut app);
    assert_eq!(
        (q.kind, q.occurrences.len()),
        (Kind::References, 1),
        "one symbol: straight to references"
    );

    // Two `foo` with one definition: no choice either, once both answered.
    app.close_locations();
    app.move_cursor(1).unwrap();
    app.ask_lsp(Kind::References).unwrap();
    let q = query(&mut app);
    assert_eq!(q.kind, Kind::Definition, "first, which symbols they are");
    answer(&mut app, &q, 0, vec![at("src/f.cpp", 1, "int foo(int)")]);
    answer(&mut app, &q, 1, vec![at("src/f.cpp", 1, "int foo(int)")]);
    let refs = query(&mut app);
    assert_eq!((refs.kind, refs.occurrences[0].1.column), (Kind::References, 2));
    assert_eq!(app.locations.as_ref().unwrap().stage, Stage::References(0));

    // Overloads: two choices, each with its signature, and `enter` asks about the picked one.
    app.close_locations();
    app.move_cursor(1).unwrap();
    app.ask_lsp(Kind::References).unwrap();
    let q = query(&mut app);
    answer(&mut app, &q, 0, vec![at("src/f.cpp", 1, "int foo(int)")]);
    answer(&mut app, &q, 1, vec![at("src/f.cpp", 2, "int foo(const char*)")]);
    assert!(app.lsp_query.is_none(), "a real choice waits for the reviewer");
    app.locations_move(1);
    app.locations_open().unwrap();
    let refs = query(&mut app);
    assert_eq!((refs.kind, refs.occurrences[0].1.column), (Kind::References, 11));
}

#[test]
fn a_line_without_symbols_or_a_file_without_a_server_asks_nothing() {
    let r = repo();
    let mut app = on(&r, "src/main.cpp", 3);
    app.ask_lsp(Kind::Definition).unwrap();
    assert!(app.lsp_query.is_none());
    assert_eq!(app.mode, Mode::Normal);
    let mut app = on(&r, "README.md", 1);
    assert!(!app.lsp_available());
    app.ask_lsp(Kind::Definition).unwrap();
    assert!(app.lsp_query.is_none());
}

#[test]
fn searching_shows_only_past_the_indicator_delay() {
    let r = repo();
    let mut app = on(&r, "src/main.cpp", 2);
    app.ask_lsp(Kind::Definition).unwrap();
    let wait = app.lsp_indicator_wait().expect("a wake is armed for the indicator");
    assert!(wait <= std::time::Duration::from_millis(200));
    let list = app.locations.as_mut().unwrap();
    list.asked_at -= std::time::Duration::from_millis(300);
    assert_eq!(app.lsp_indicator_wait(), None, "past it, nothing more to wake for");
}

#[test]
fn a_file_changed_on_disk_reads_as_a_notice_not_as_answers() {
    let r = repo();
    let mut app = on(&r, "src/main.cpp", 2);
    app.ask_lsp(Kind::Definition).unwrap();
    let q = query(&mut app);
    app.apply_lsp_completion(LspCompletion {
        tag: q.tag,
        occurrence: None,
        outcome: Outcome::Mismatch,
    });
    let list = app.locations.as_ref().unwrap();
    assert!(list.rows().is_empty());
    assert!(
        list.notice.as_deref().is_some_and(|n| n.contains("changed on disk")),
        "{:?}",
        list.notice
    );
}

#[test]
fn from_changes_the_press_lands_in_the_worktree_file_and_asks_there() {
    let r = repo();
    r.write("src/main.cpp", "int main() {\n  auto user = loadUser(id);\n  return 0;\n}\n");
    let mut app = app_on(&r);
    let row = app
        .file_rows
        .iter()
        .position(|row| row.file_index().is_some_and(|i| app.entries[i].path == "src/main.cpp"));
    app.select_file(row.unwrap()).unwrap();
    app.diff_cursor = app.visible.iter().position(|row| row.text().contains("loadUser")).unwrap();
    app.ask_lsp(Kind::Definition).unwrap();
    assert_eq!((app.tab, app.diff_path.as_deref()), (Tab::AllFiles, Some("src/main.cpp")));
    assert_eq!(app.visible[app.diff_cursor].text(), "  auto user = loadUser(id);");
    let q = query(&mut app);
    assert_eq!((q.tag.line, q.occurrences.len()), (2, 4));
    // A keyword is never asked about: `return 0` holds no symbol.
    app.diff_cursor += 1;
    app.ask_lsp(Kind::Definition).unwrap();
    assert!(app.lsp_query.is_none());
    assert_eq!(app.status, "no symbol on this line");
}

#[test]
fn from_changes_a_line_that_reads_differently_on_disk_lands_without_asking() {
    let r = repo();
    r.write("src/main.cpp", "int main() {\n  auto user = loadUser(id);\n  return 0;\n}\n");
    let mut app = app_on(&r);
    let row = app
        .file_rows
        .iter()
        .position(|row| row.file_index().is_some_and(|i| app.entries[i].path == "src/main.cpp"));
    app.select_file(row.unwrap()).unwrap();
    app.diff_cursor = app.visible.iter().position(|row| row.text().contains("return 0")).unwrap();
    // The agent edits the line after the diff was read.
    r.write("src/main.cpp", "int main() {\n  auto user = loadUser(id);\n  return 1;\n}\n");
    app.ask_lsp(Kind::Definition).unwrap();
    assert_eq!(app.tab, Tab::AllFiles, "the press still lands");
    assert!(app.lsp_query.is_none() && app.mode == Mode::Normal);
    assert!(app.status.contains("differently on disk"), "{}", app.status);
}

#[test]
fn a_deleted_line_or_a_file_without_a_server_asks_nothing_and_says_why() {
    let r = repo();
    r.write("src/main.cpp", "int main() {\n}\n");
    let mut app = app_on(&r);
    let row = app
        .file_rows
        .iter()
        .position(|row| row.file_index().is_some_and(|i| app.entries[i].path == "src/main.cpp"));
    app.select_file(row.unwrap()).unwrap();
    app.diff_cursor = app.visible.iter().position(|row| row.text().contains("loadUser")).unwrap();
    app.ask_lsp(Kind::Definition).unwrap();
    assert_eq!(app.tab, Tab::Changes, "a deleted line is not in the worktree");
    assert!(app.status.contains("follow the worktree"), "{}", app.status);
    assert!(app.lsp_query.is_none());

    let mut app = on(&r, "README.md", 1);
    app.ask_lsp(Kind::Definition).unwrap();
    assert!(app.lsp_query.is_none() && app.status.contains("no language server"), "{}", app.status);
}

#[test]
fn the_commits_scope_shows_no_worktree_lines_to_ask_about() {
    let r = repo();
    r.write("src/main.cpp", "int main() {\n  auto user = loadUser(id);\n  return 0;\n}\n");
    r.commit_all("return");
    let mut app = app_on(&r);
    app.open_commit_picker();
    app.commit_picker_pick().unwrap();
    assert_eq!(app.scope, herdr_reviewr::model::Scope::Commits);
    let row = app
        .file_rows
        .iter()
        .position(|row| row.file_index().is_some_and(|i| app.entries[i].path == "src/main.cpp"));
    app.select_file(row.unwrap()).unwrap();
    app.diff_cursor = app.visible.iter().position(|row| row.text().contains("return 0")).unwrap();
    app.ask_lsp(Kind::Definition).unwrap();
    assert_eq!(app.tab, Tab::Changes);
    assert!(app.status.contains("follow the worktree"), "{}", app.status);
    assert!(app.lsp_query.is_none());
}

#[test]
fn a_view_older_than_the_disk_asks_nothing_and_says_so() {
    let r = repo();
    let mut app = on(&r, "src/main.cpp", 2);
    // The agent edits after the view was read; the next refresh has not landed.
    r.write("src/main.cpp", "int main() {\n  auto user = loadUser(other);\n}\n");
    app.ask_lsp(Kind::Definition).unwrap();
    assert!(app.lsp_query.is_none() && app.locations.is_none() && app.mode == Mode::Normal);
    assert!(app.status.contains("changed on disk"), "{}", app.status);
}

#[test]
fn a_hidden_pane_arms_no_indicator_wake() {
    let r = repo();
    let mut app = on(&r, "src/main.cpp", 2);
    app.ask_lsp(Kind::Definition).unwrap();
    app.sync_herdr_session(None, false);
    assert_eq!(app.lsp_indicator_wait(), None);
}

/// The pane jumps from the cursor's line to `path:line` through the list, as `D` then `enter`.
fn jump(app: &mut App, path: &str, line: u32) {
    app.ask_lsp(Kind::Definition).unwrap();
    let q = query(app);
    let (index, _) = q.occurrences[0].clone();
    answer(app, &q, index, vec![at(path, line, "")]);
    app.locations_open().unwrap();
    assert_eq!(app.mode, Mode::Normal, "the jump landed");
}

fn chain_repo() -> Repo {
    let r = Repo::init();
    r.write("a.go", "package a\n\nfunc A() { B() }\n");
    r.write("b.go", "package a\n\nfunc B() { C() }\n");
    r.write("c.go", "package a\n\nfunc C() {}\n");
    r.commit_all("init");
    r
}

fn at_line(app: &App) -> (Tab, Option<String>, Option<u32>) {
    (
        app.tab,
        app.diff_path.clone(),
        app.visible.get(app.diff_cursor).and_then(herdr_reviewr::diff::Row::new_no),
    )
}

#[test]
fn back_and_forward_walk_a_chain_of_jumps() {
    let r = chain_repo();
    let mut app = on(&r, "a.go", 3);
    jump(&mut app, "b.go", 3);
    jump(&mut app, "c.go", 3);
    app.walk_history(false).unwrap();
    assert_eq!(at_line(&app), (Tab::AllFiles, Some("b.go".into()), Some(3)));
    app.walk_history(false).unwrap();
    assert_eq!(at_line(&app), (Tab::AllFiles, Some("a.go".into()), Some(3)));
    app.walk_history(false).unwrap();
    assert_eq!(at_line(&app).1.as_deref(), Some("a.go"), "nothing further back");
    app.walk_history(true).unwrap();
    app.walk_history(true).unwrap();
    assert_eq!(at_line(&app).1.as_deref(), Some("c.go"));
}

#[test]
fn a_new_jump_from_the_middle_drops_the_way_forward() {
    let r = chain_repo();
    let mut app = on(&r, "a.go", 3);
    jump(&mut app, "b.go", 3);
    jump(&mut app, "c.go", 3);
    app.walk_history(false).unwrap();
    jump(&mut app, "a.go", 1);
    assert!(!app.history.can_forward());
    app.walk_history(true).unwrap();
    assert_eq!(at_line(&app).1.as_deref(), Some("a.go"), "forward goes nowhere");
}

#[test]
fn back_finds_the_line_by_its_text_after_edits_above_and_clamps_when_it_is_gone() {
    let r = chain_repo();
    let mut app = on(&r, "a.go", 3);
    jump(&mut app, "b.go", 3);
    r.write("a.go", &format!("package a\n{}\nfunc A() {{ B() }}\n", "\n".repeat(10)));
    app.walk_history(false).unwrap();
    assert_eq!(at_line(&app), (Tab::AllFiles, Some("a.go".into()), Some(13)));
    jump(&mut app, "b.go", 3);
    r.write("a.go", "package a\n");
    app.walk_history(false).unwrap();
    assert_eq!(at_line(&app), (Tab::AllFiles, Some("a.go".into()), Some(1)), "clamped to the file");
}

#[test]
fn back_skips_a_place_whose_file_is_gone() {
    let r = chain_repo();
    let mut app = on(&r, "a.go", 3);
    jump(&mut app, "b.go", 3);
    jump(&mut app, "c.go", 3);
    r.remove("b.go");
    app.walk_history(false).unwrap();
    assert_eq!(at_line(&app).1.as_deref(), Some("a.go"));
}

#[test]
fn back_returns_to_the_changes_row_the_jump_left_from() {
    let r = chain_repo();
    r.write("a.go", "package a\n\nfunc A() { B(); B() }\n");
    let mut app = app_on(&r);
    app.diff_cursor = app.visible.iter().position(|row| row.text().contains("B(); B()")).unwrap();
    jump(&mut app, "b.go", 3);
    app.walk_history(false).unwrap();
    assert_eq!(at_line(&app), (Tab::Changes, Some("a.go".into()), Some(3)));
    // Gone from the changes by the time `back` is pressed, the same place opens in the worktree.
    jump(&mut app, "b.go", 3);
    r.write("a.go", "package a\n\nfunc A() { B(); B() }\n// tail\n");
    r.commit_all("tail");
    // The `Changes` list has heard: a.go left it.
    enter_tab(&mut app, Tab::Changes);
    enter_tab(&mut app, Tab::AllFiles);
    app.walk_history(false).unwrap();
    assert_eq!(at_line(&app), (Tab::AllFiles, Some("a.go".into()), Some(3)));
}

/// Thirty comment lines, enough to fold between two far edits.
fn filler() -> String {
    (1..=30).map(|i| ["// ", &i.to_string(), "\n"].concat()).collect()
}

#[test]
fn a_changes_place_whose_line_left_the_diff_opens_in_the_worktree() {
    let r = chain_repo();
    let lines = filler();
    r.write("a.go", &format!("package a\n{lines}func A() {{ B() }}\n"));
    r.commit_all("long");
    // Two edits far apart: the file stays listed when one of them is undone.
    r.write("a.go", &format!("package a // top\n{lines}func A() {{ B(); B() }}\n"));
    let mut app = app_on(&r);
    app.diff_cursor = app.visible.iter().position(|row| row.text().contains("B(); B()")).unwrap();
    jump(&mut app, "b.go", 3);
    r.write("a.go", &format!("package a // top\n{lines}func A() {{ B() }}\n"));
    enter_tab(&mut app, Tab::Changes);
    enter_tab(&mut app, Tab::AllFiles);
    app.walk_history(false).unwrap();
    assert_eq!(at_line(&app).0, Tab::AllFiles, "the line is no longer a change");
    assert_eq!(at_line(&app).1.as_deref(), Some("a.go"));
}

#[test]
fn a_place_taken_on_a_fold_keeps_its_text() {
    let r = chain_repo();
    let lines = filler();
    r.write("a.go", &format!("package a\n{lines}func A() {{ B() }}\n"));
    r.commit_all("long");
    r.write("a.go", &format!("package a // top\n{lines}func A() {{ B(); B() }}\n"));
    let mut app = app_on(&r);
    app.diff_cursor = app.visible.iter().position(|row| row.text().contains("B(); B()")).unwrap();
    jump(&mut app, "b.go", 3);
    app.walk_history(false).unwrap();
    // On the fold between the two edits, `forward` then `back` lands on its first hidden line.
    app.diff_cursor = app
        .visible
        .iter()
        .position(|row| matches!(row, herdr_reviewr::diff::Row::Fold { .. }))
        .unwrap();
    app.walk_history(true).unwrap();
    app.walk_history(false).unwrap();
    let row = &app.visible[app.diff_cursor];
    assert!(row.text().starts_with("// "), "the folded line, opened: {:?}", row.text());
}
