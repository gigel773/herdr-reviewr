//! Where `D` and `R` lead: the list they open, filled as answers land, and the history of jumps.

use crate::lsp::uri::Located;
use crate::lsp::{Kind, Location, Occurrence, Outcome, Query, Tag};

/// What the list is showing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Each occurrence's definitions; for `R`, the choice of which symbol to look up.
    Definitions,
    /// The references to the occurrence at this index.
    References(usize),
}

/// One painted row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row<'a> {
    /// An occurrence and the repeats folded into it; `R`'s choice shows its first definition.
    Group {
        name: &'a str,
        occurrence: usize,
        pending: bool,
        signature: Option<&'a str>,
    },
    Place {
        occurrence: usize,
        location: &'a Location,
    },
}

/// What the pick holds on to: a row's identity, never its index.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Pick {
    Group(usize),
    Place(usize, Located, u32),
}

/// The list's whole state, dropped on close.
#[derive(Debug)]
pub struct Locations {
    /// What the reviewer asked: `D` or `R`.
    pub kind: Kind,
    pub stage: Stage,
    /// The live query's identity; an answer for any other never lands.
    pub tag: Tag,
    pub occurrences: Vec<Occurrence>,
    /// Each occurrence's definitions, `None` while asked.
    definitions: Vec<Option<Vec<Location>>>,
    /// The references stage's answer, `None` while asked.
    references: Option<Vec<Location>>,
    /// Why some or all of the query has nothing to show.
    pub notice: Option<String>,
    /// The query as a whole was answered with nothing: no more answers come.
    ended: bool,
    pick: Option<Pick>,
    /// Top visible row, kept by the renderer so the pick stays in view.
    pub scroll: std::cell::Cell<usize>,
    pub asked_at: std::time::Instant,
}

impl Locations {
    pub fn new(kind: Kind, tag: Tag, occurrences: Vec<Occurrence>) -> Self {
        let definitions = vec![None; occurrences.len()];
        // `R` on a line with one symbol has nothing to choose.
        let stage = if kind == Kind::References && occurrences.len() == 1 {
            Stage::References(0)
        } else {
            Stage::Definitions
        };
        Self {
            kind,
            stage,
            tag,
            occurrences,
            definitions,
            references: None,
            notice: None,
            ended: false,
            pick: None,
            scroll: std::cell::Cell::new(0),
            asked_at: std::time::Instant::now(),
        }
    }

    /// Whether a query is still out.
    pub fn pending(&self) -> bool {
        !self.ended
            && match self.stage {
                Stage::Definitions => self.definitions.iter().any(Option::is_none),
                Stage::References(_) => self.references.is_none(),
            }
    }

    /// Land one answer; `None` occurrence speaks for the whole query.
    pub fn land(&mut self, occurrence: Option<usize>, outcome: Outcome) {
        let found = match outcome {
            Outcome::Ready(found) => found,
            other => {
                self.notice = Some(notice(&other));
                Vec::new()
            }
        };
        // The whole query answered: nothing still asked is coming.
        if occurrence.is_none() {
            self.ended = true;
            self.definitions.iter_mut().filter(|d| d.is_none()).for_each(|d| *d = Some(Vec::new()));
            self.references.get_or_insert_with(Vec::new);
        }
        match (self.stage, occurrence) {
            (Stage::Definitions, Some(i)) if i < self.definitions.len() => {
                self.definitions[i] = Some(found);
            }
            (Stage::References(_), Some(_)) => self.references = Some(found),
            _ => {}
        }
        self.settle();
    }

    /// The groups in line order: each occurrence's first, later ones with equal places folded in.
    fn groups(&self) -> Vec<usize> {
        let mut groups: Vec<usize> = Vec::new();
        for (i, found) in self.definitions.iter().enumerate() {
            let Some(found) = found else {
                groups.push(i);
                continue;
            };
            let name = &self.occurrences[i].name;
            let same = |&g: &usize| {
                self.occurrences[g].name == *name
                    && self.definitions[g].as_deref().is_some_and(|d| same_places(d, found))
            };
            // No places: the symbol leaves `D`'s list, yet `R` may still look it up.
            let shown = !found.is_empty() || (self.kind == Kind::References && !self.ended);
            if shown && !groups.iter().any(same) {
                groups.push(i);
            }
        }
        groups
    }

    /// The group an occurrence was folded into.
    fn group_of(&self, occurrence: usize) -> Option<usize> {
        let groups = self.groups();
        if groups.contains(&occurrence) {
            return Some(occurrence);
        }
        let found = self.definitions.get(occurrence)?.as_ref()?;
        let name = &self.occurrences.get(occurrence)?.name;
        groups.into_iter().find(|&g| {
            self.occurrences[g].name == *name
                && self.definitions[g].as_deref().is_some_and(|d| same_places(d, found))
        })
    }

    pub fn rows(&self) -> Vec<Row<'_>> {
        if let Stage::References(occurrence) = self.stage {
            let found = self.references.iter().flatten();
            return found.map(|location| Row::Place { occurrence, location }).collect();
        }
        let mut rows = Vec::new();
        for g in self.groups() {
            let found = self.definitions[g].as_deref();
            let name = &self.occurrences[g].name;
            let choosing = self.kind == Kind::References;
            let signature =
                found.filter(|_| choosing).and_then(|f| f.first()).map(|l| l.text.as_str());
            rows.push(Row::Group { name, occurrence: g, pending: found.is_none(), signature });
            // `R` chooses a symbol here: its places come in the next stage.
            if self.kind == Kind::Definition {
                rows.extend(
                    found
                        .into_iter()
                        .flatten()
                        .map(|location| Row::Place { occurrence: g, location }),
                );
            }
        }
        rows
    }

    fn pickable(&self, row: &Row<'_>) -> bool {
        match row {
            Row::Place { .. } => true,
            Row::Group { pending, .. } => self.kind == Kind::References && !pending,
        }
    }

    fn identity(row: &Row<'_>) -> Pick {
        match row {
            Row::Group { occurrence, .. } => Pick::Group(*occurrence),
            Row::Place { occurrence, location } => {
                Pick::Place(*occurrence, location.place.clone(), location.line)
            }
        }
    }

    /// The picked row's index among [`Locations::rows`].
    pub fn picked(&self) -> Option<usize> {
        let pick = self.pick.as_ref()?;
        self.rows().iter().position(|row| Self::identity(row) == *pick)
    }

    /// The picked place, for `enter` and `e`.
    pub fn picked_place(&self) -> Option<&Location> {
        match *self.rows().get(self.picked()?)? {
            Row::Place { location, .. } => Some(location),
            Row::Group { .. } => None,
        }
    }

    /// The picked group's occurrence, for `R`'s choice.
    pub fn picked_group(&self) -> Option<usize> {
        match self.pick {
            Some(Pick::Group(occurrence)) => Some(occurrence),
            _ => None,
        }
    }

    /// Keep the pick on its identity as rows fill and fold, or on the first pickable row.
    fn settle(&mut self) {
        let rows = self.rows();
        let kept = self.pick.clone().and_then(|pick| {
            let pick = match pick {
                Pick::Group(o) => Pick::Group(self.group_of(o).unwrap_or(o)),
                Pick::Place(o, place, line) => {
                    Pick::Place(self.group_of(o).unwrap_or(o), place, line)
                }
            };
            rows.iter().any(|row| self.pickable(row) && Self::identity(row) == pick).then_some(pick)
        });
        self.pick = kept.or_else(|| rows.iter().find(|row| self.pickable(row)).map(Self::identity));
    }

    pub fn step(&mut self, delta: isize) {
        let rows = self.rows();
        let pickable: Vec<usize> = (0..rows.len()).filter(|&i| self.pickable(&rows[i])).collect();
        if pickable.is_empty() {
            return;
        }
        let at = self.picked().and_then(|p| pickable.iter().position(|&i| i == p)).unwrap_or(0);
        let to = at.saturating_add_signed(delta).min(pickable.len() - 1);
        self.pick = Some(Self::identity(&rows[pickable[to]]));
    }

    /// `R`'s choice made: references to `occurrence`, asked under `tag`.
    pub fn choose(&mut self, occurrence: usize, tag: Tag) {
        self.stage = Stage::References(occurrence);
        self.tag = tag;
        self.notice = None;
        self.ended = false;
        self.pick = None;
        self.scroll.set(0);
        self.asked_at = std::time::Instant::now();
    }

    /// What the current stage still waits for, as the worker is asked it.
    pub fn query(&self) -> Query {
        let (kind, occurrences) = match self.stage {
            Stage::Definitions => {
                let unanswered = self.occurrences.iter().cloned().enumerate();
                (
                    Kind::Definition,
                    unanswered.filter(|(i, _)| self.definitions[*i].is_none()).collect(),
                )
            }
            Stage::References(i) => (Kind::References, vec![(i, self.occurrences[i].clone())]),
        };
        Query { tag: self.tag.clone(), kind, occurrences }
    }

    /// Ask again under `tag`: the wait starts over, so a fast answer never flashes `searching…`.
    pub fn reask(&mut self, tag: Tag) {
        self.tag = tag;
        self.asked_at = std::time::Instant::now();
    }

    /// `R`'s one remaining choice once every definition is in, when there is just one.
    pub fn sole_choice(&self) -> Option<usize> {
        let settled = self.kind == Kind::References && self.stage == Stage::Definitions;
        let groups = self.groups();
        (settled && !self.pending() && groups.len() == 1).then(|| groups[0])
    }
}

/// A place the history returns to, by identity: a line is found again by its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    pub tab: crate::app::Tab,
    pub path: String,
    /// 1-based source line.
    pub line: u32,
    pub text: String,
}

/// The most places each direction keeps; the oldest goes first.
const HISTORY: usize = 100;

/// `back` and `forward`, as a browser keeps them.
#[derive(Debug, Default)]
pub struct History {
    back: Vec<Place>,
    forward: Vec<Place>,
}

impl History {
    /// A jump left `from`: it is the way back, and the old way forward is gone.
    pub fn jumped(&mut self, from: Place) {
        push(&mut self.back, from);
        self.forward.clear();
    }

    /// The next place one way, taken off; the caller keeps where it left on the other.
    pub fn take(&mut self, forward: bool) -> Option<Place> {
        if forward { self.forward.pop() } else { self.back.pop() }
    }

    /// Keep `left` as the way back from a `forward` walk, or the way forward from a `back` one.
    pub fn left(&mut self, left: Place, forward: bool) {
        push(if forward { &mut self.back } else { &mut self.forward }, left);
    }

    pub fn can_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_forward(&self) -> bool {
        !self.forward.is_empty()
    }
}

fn push(places: &mut Vec<Place>, place: Place) {
    if places.len() == HISTORY {
        places.remove(0);
    }
    places.push(place);
}

/// The status a whole-query outcome reads as.
fn notice(outcome: &Outcome) -> String {
    match outcome {
        Outcome::NoServer { name, hint } => format!("{name} not found: {hint}"),
        Outcome::Mismatch => "the file changed on disk: refresh, then ask again".into(),
        Outcome::Unsupported => "no language server for this file".into(),
        Outcome::Failed(message) => message.clone(),
        Outcome::Ready(_) => String::new(),
    }
}

/// Whether two answers point at the same places in any order, whatever text they read there.
fn same_places(a: &[Location], b: &[Location]) -> bool {
    // The worker drops repeats, so equal lengths and containment make equal sets.
    a.len() == b.len() && a.iter().all(|x| b.iter().any(|y| x.spot() == y.spot()))
}

#[cfg(test)]
mod tests {
    use super::{Locations, Row, Stage};
    use crate::lsp::{Context, Kind, Location, Occurrence, Outcome, Tag, uri::Located};

    fn tag() -> Tag {
        Tag { generation: 1, path: "a.cpp".into(), hash: 0, line: 1 }
    }

    fn at(path: &str, line: u32, text: &str) -> Location {
        Location {
            place: Located::Repo(path.into()),
            line,
            text: text.into(),
            context: Context::default(),
        }
    }

    fn list(kind: Kind, names: &[&str]) -> Locations {
        let occurrences = names
            .iter()
            .enumerate()
            .map(|(i, n)| Occurrence { name: (*n).into(), column: i * 10 })
            .collect();
        Locations::new(kind, tag(), occurrences)
    }

    fn shown(l: &Locations) -> Vec<String> {
        l.rows()
            .iter()
            .map(|row| match row {
                Row::Group { name, pending: true, .. } => format!("{name}…"),
                Row::Group { name, signature: Some(s), .. } => format!("{name}  {s}"),
                Row::Group { name, .. } => (*name).to_string(),
                Row::Place { location, .. } => format!("  {}", location.text),
            })
            .collect()
    }

    #[test]
    fn groups_fill_in_line_order_and_the_pick_stays_put() {
        let mut l = list(Kind::Definition, &["loadUser", "id"]);
        assert_eq!(shown(&l), ["loadUser…", "id…"]);
        assert_eq!(l.picked(), None, "nothing pickable while asked");
        l.land(Some(1), Outcome::Ready(vec![at("a.cpp", 3, "int id")]));
        assert_eq!(l.picked_place().map(|p| p.line), Some(3));
        // An answer landing above never moves the pick.
        l.land(Some(0), Outcome::Ready(vec![at("user.cpp", 42, "User loadUser(int)")]));
        assert_eq!(shown(&l), ["loadUser", "  User loadUser(int)", "id", "  int id"]);
        assert_eq!(l.picked_place().map(|p| p.line), Some(3));
        l.step(-1);
        assert_eq!(l.picked_place().map(|p| p.line), Some(42));
        assert!(!l.pending());
    }

    #[test]
    fn a_place_is_its_path_and_line_whatever_text_reads_there() {
        let mut l = list(Kind::Definition, &["foo", "foo"]);
        l.land(Some(0), Outcome::Ready(vec![at("f.cpp", 1, "int foo(int)")]));
        // The second answer read the file after an edit: same place, new text.
        l.land(Some(1), Outcome::Ready(vec![at("f.cpp", 1, "int foo(long)")]));
        assert_eq!(shown(&l), ["foo", "  int foo(int)"], "one place folds into one group");
        // A declaration and a definition, answered in either order, are one set.
        let mut l = list(Kind::Definition, &["foo", "foo"]);
        let (decl, def) = (at("f.h", 3, "int foo();"), at("f.cpp", 9, "int foo() {"));
        l.land(Some(0), Outcome::Ready(vec![decl.clone(), def.clone()]));
        l.land(Some(1), Outcome::Ready(vec![def, decl]));
        assert_eq!(shown(&l), ["foo", "  int foo();", "  int foo() {"]);
    }

    #[test]
    fn a_symbol_without_definitions_leaves_and_one_place_is_picked_at_once() {
        let mut l = list(Kind::Definition, &["auto", "x"]);
        l.land(Some(0), Outcome::Ready(Vec::new()));
        l.land(Some(1), Outcome::Ready(vec![at("a.cpp", 1, "int x")]));
        assert_eq!(shown(&l), ["x", "  int x"]);
        assert_eq!(l.picked_place().map(|p| p.line), Some(1));
    }

    #[test]
    fn repeats_fold_when_they_name_the_same_places_and_overloads_stay_apart() {
        let mut same = list(Kind::Definition, &["foo", "foo"]);
        same.land(Some(1), Outcome::Ready(vec![at("f.cpp", 1, "int foo(int)")]));
        same.step(1);
        same.land(Some(0), Outcome::Ready(vec![at("f.cpp", 1, "int foo(int)")]));
        assert_eq!(shown(&same), ["foo", "  int foo(int)"]);
        // The pick on the second `foo` stays on the group it folded into.
        assert_eq!(same.picked_place().map(|p| p.line), Some(1));

        let mut overloads = list(Kind::Definition, &["foo", "foo"]);
        overloads.land(Some(0), Outcome::Ready(vec![at("f.cpp", 1, "int foo(int)")]));
        overloads.land(Some(1), Outcome::Ready(vec![at("f.cpp", 2, "int foo(const char*)")]));
        assert_eq!(shown(&overloads), ["foo", "  int foo(int)", "foo", "  int foo(const char*)"]);
    }

    #[test]
    fn references_choose_a_group_then_list_its_places() {
        let mut one = list(Kind::References, &["foo"]);
        assert_eq!(one.stage, Stage::References(0), "one symbol: no choice");
        one.land(Some(0), Outcome::Ready(vec![at("a.cpp", 1, "foo()"), at("b.cpp", 9, "foo()")]));
        assert_eq!(one.rows().len(), 2);

        let mut two = list(Kind::References, &["foo", "foo"]);
        assert_eq!(two.sole_choice(), None);
        two.land(Some(0), Outcome::Ready(vec![at("f.cpp", 1, "int foo(int)")]));
        two.land(Some(1), Outcome::Ready(vec![at("f.cpp", 1, "int foo(int)")]));
        assert_eq!(shown(&two), ["foo  int foo(int)"], "a choice lists symbols, not places");
        assert_eq!(two.sole_choice(), Some(0), "one definition: no choice either");
        two.choose(0, tag());
        assert!(two.pending());

        let mut overloads = list(Kind::References, &["foo", "foo"]);
        overloads.land(Some(0), Outcome::Ready(vec![at("f.cpp", 1, "int foo(int)")]));
        overloads.land(Some(1), Outcome::Ready(vec![at("f.cpp", 2, "int foo(const char*)")]));
        assert_eq!(shown(&overloads), ["foo  int foo(int)", "foo  int foo(const char*)"]);
    }

    #[test]
    fn references_can_look_up_a_symbol_the_server_has_no_definition_for() {
        let mut l = list(Kind::References, &["a", "b"]);
        l.land(Some(0), Outcome::Ready(Vec::new()));
        l.land(Some(1), Outcome::Ready(Vec::new()));
        assert_eq!(shown(&l), ["a", "b"]);
        assert_eq!(l.picked_group(), Some(0));
        l.step(1);
        assert_eq!(l.picked_group(), Some(1));
    }

    #[test]
    fn one_symbol_failing_says_so_and_the_rest_still_come() {
        let mut l = list(Kind::Definition, &["x", "y"]);
        l.land(Some(0), Outcome::Failed("timed out".into()));
        assert!(l.pending());
        assert_eq!(l.notice.as_deref(), Some("timed out"));
        l.land(Some(1), Outcome::Ready(vec![at("a.cpp", 1, "int y")]));
        assert_eq!(shown(&l), ["y", "  int y"]);
    }

    #[test]
    fn asking_again_asks_only_what_is_still_out_under_its_own_index() {
        let mut l = list(Kind::Definition, &["x", "y", "z"]);
        l.land(Some(1), Outcome::Ready(vec![at("a.cpp", 1, "int y")]));
        let asked: Vec<usize> = l.query().occurrences.iter().map(|(i, _)| *i).collect();
        assert_eq!(asked, [0, 2]);
        l.asked_at -= std::time::Duration::from_secs(1);
        l.reask(Tag { generation: 2, ..tag() });
        assert!(l.asked_at.elapsed() < std::time::Duration::from_secs(1), "the wait starts over");
        let mut refs = list(Kind::References, &["a", "b"]);
        refs.choose(1, tag());
        assert_eq!(refs.query().occurrences.iter().map(|(i, _)| *i).collect::<Vec<_>>(), [1]);
    }

    #[test]
    fn history_walks_back_and_forward_and_a_new_jump_drops_the_way_forward() {
        use super::{History, Place};
        let at = |line| Place {
            tab: crate::app::Tab::AllFiles,
            path: "a.cpp".into(),
            line,
            text: String::new(),
        };
        let mut h = History::default();
        h.jumped(at(1));
        h.jumped(at(2));
        // At 3: back to 2, back to 1, forward to 2.
        let walk = |h: &mut History, from: u32, forward: bool| {
            let to = h.take(forward);
            if to.is_some() {
                h.left(at(from), forward);
            }
            to.map(|p| p.line)
        };
        assert_eq!(walk(&mut h, 3, false), Some(2));
        assert_eq!(walk(&mut h, 2, false), Some(1));
        assert_eq!(walk(&mut h, 1, false), None);
        assert_eq!(walk(&mut h, 1, true), Some(2));
        // From 2, a new jump: 3 is no longer ahead.
        h.jumped(at(2));
        assert!(!h.can_forward());
        for line in 0..200 {
            h.jumped(at(line));
        }
        assert_eq!(h.back.len(), 100, "capped, the oldest gone");
        assert_eq!(h.back[0].line, 100);
    }

    #[test]
    fn a_whole_query_outcome_is_a_notice_and_ends_the_wait() {
        let mut l = list(Kind::Definition, &["x", "y"]);
        l.land(None, Outcome::NoServer { name: "clangd", hint: "install clangd" });
        assert!(!l.pending());
        assert!(l.rows().is_empty(), "no row is left waiting");
        assert_eq!(l.notice.as_deref(), Some("clangd not found: install clangd"));
    }
}
