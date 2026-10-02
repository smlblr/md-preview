use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentTab {
    pub id: u64,
    pub path: PathBuf,
    pub dirty: bool,
    pub missing: bool,
    pub edit_on_open: bool,
    pub group: Option<u64>,
}

/// Number of entries in the tab bar's group palette (`.gc0` … `.gc7` in the page CSS).
pub const GROUP_COLOR_COUNT: u8 = 8;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct TabGroup {
    pub id: u64,
    pub name: String,
    pub color: u8,
    pub collapsed: bool,
}

/// Tab bar rearrangements that never change which document is shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TabLayoutChange {
    Move {
        id: u64,
        before: Option<u64>,
        group: Option<u64>,
    },
    NewGroup(u64),
    AssignGroup(u64, Option<u64>),
    RenameGroup(u64, String),
    RecolorGroup(u64, u8),
    ToggleGroup(u64),
    Ungroup(u64),
}

#[derive(Debug, Default)]
pub struct DocumentSession {
    pub tabs: Vec<DocumentTab>,
    pub groups: Vec<TabGroup>,
    pub active_id: Option<u64>,
    next_id: u64,
    next_group_id: u64,
}

#[derive(Deserialize)]
struct VersionProbe {
    version: u8,
}

#[derive(Deserialize)]
struct PersistedSessionV1 {
    active: Option<usize>,
    tabs: Vec<PathBuf>,
}

#[derive(Deserialize, Serialize)]
struct PersistedSession {
    version: u8,
    active: Option<usize>,
    tabs: Vec<PersistedTab>,
    groups: Vec<TabGroup>,
}

#[derive(Deserialize, Serialize)]
struct PersistedTab {
    path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    group: Option<u64>,
}

impl DocumentSession {
    pub fn load(path: &Path) -> Self {
        let Ok(raw) = fs::read(path) else {
            return Self::default();
        };
        let Ok(probe) = serde_json::from_slice::<VersionProbe>(&raw) else {
            return Self::default();
        };
        let saved = match probe.version {
            1 => match serde_json::from_slice::<PersistedSessionV1>(&raw) {
                Ok(v1) => PersistedSession {
                    version: 2,
                    active: v1.active,
                    tabs: v1
                        .tabs
                        .into_iter()
                        .map(|path| PersistedTab { path, group: None })
                        .collect(),
                    groups: Vec::new(),
                },
                Err(_) => return Self::default(),
            },
            2 => match serde_json::from_slice::<PersistedSession>(&raw) {
                Ok(v2) => v2,
                Err(_) => return Self::default(),
            },
            _ => return Self::default(),
        };

        let mut session = Self::default();
        for mut group in saved.groups {
            if session.group(group.id).is_some() {
                continue;
            }
            group.color %= GROUP_COLOR_COUNT;
            session.next_group_id = session.next_group_id.max(group.id);
            session.groups.push(group);
        }
        let mut opened = Vec::new();
        for tab in saved.tabs {
            let id = session.open(tab.path, false);
            let group = tab.group.filter(|group| session.group(*group).is_some());
            if let Some(tab) = session.get_mut(id) {
                tab.group = group;
            }
            opened.push(id);
        }
        session.normalize_groups();
        session.active_id = saved
            .active
            .and_then(|index| opened.get(index).copied())
            .or_else(|| session.tabs.last().map(|tab| tab.id));
        session
    }

    pub fn open(&mut self, path: PathBuf, edit_on_open: bool) -> u64 {
        let path = normalize_path(path);
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.path == path) {
            tab.missing = !tab.path.exists();
            tab.edit_on_open |= edit_on_open;
            self.active_id = Some(tab.id);
            return tab.id;
        }

        self.next_id += 1;
        let id = self.next_id;
        self.tabs.push(DocumentTab {
            id,
            missing: !path.exists(),
            path,
            dirty: false,
            edit_on_open,
            group: None,
        });
        self.active_id = Some(id);
        id
    }

    pub fn activate(&mut self, id: u64) -> bool {
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) else {
            return false;
        };
        tab.missing = !tab.path.exists();
        self.active_id = Some(id);
        true
    }

    pub fn close(&mut self, id: u64) -> bool {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return false;
        };
        let was_active = self.active_id == Some(id);
        self.tabs.remove(index);
        self.prune_empty_groups();
        if was_active {
            self.active_id = self
                .tabs
                .get(index)
                .or_else(|| index.checked_sub(1).and_then(|i| self.tabs.get(i)))
                .map(|tab| tab.id);
        }
        true
    }

    /// Closes every tab of a group; returns false when the group does not exist.
    pub fn close_group(&mut self, group: u64) -> bool {
        let ids = self
            .tabs
            .iter()
            .filter(|tab| tab.group == Some(group))
            .map(|tab| tab.id)
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return false;
        }
        for id in ids {
            self.close(id);
        }
        true
    }

    pub fn group(&self, id: u64) -> Option<&TabGroup> {
        self.groups.iter().find(|group| group.id == id)
    }

    fn group_mut(&mut self, id: u64) -> Option<&mut TabGroup> {
        self.groups.iter_mut().find(|group| group.id == id)
    }

    /// Applies a tab bar change; returns whether anything changed.
    pub fn apply_layout(&mut self, change: TabLayoutChange) -> bool {
        let before = (self.tab_order(), self.groups.clone());
        match change {
            TabLayoutChange::Move { id, before, group } => self.move_tab(id, before, group),
            TabLayoutChange::NewGroup(id) => self.new_group(id),
            TabLayoutChange::AssignGroup(id, group) => self.assign_group(id, group),
            TabLayoutChange::RenameGroup(id, name) => {
                if let Some(group) = self.group_mut(id) {
                    group.name = name.trim().to_string();
                }
            }
            TabLayoutChange::RecolorGroup(id, color) => {
                if color < GROUP_COLOR_COUNT {
                    if let Some(group) = self.group_mut(id) {
                        group.color = color;
                    }
                }
            }
            TabLayoutChange::ToggleGroup(id) => {
                if let Some(group) = self.group_mut(id) {
                    group.collapsed = !group.collapsed;
                }
            }
            TabLayoutChange::Ungroup(id) => {
                for tab in self.tabs.iter_mut().filter(|tab| tab.group == Some(id)) {
                    tab.group = None;
                }
                self.prune_empty_groups();
            }
        }
        (self.tab_order(), self.groups.clone()) != before
    }

    fn tab_order(&self) -> Vec<(u64, Option<u64>)> {
        self.tabs.iter().map(|tab| (tab.id, tab.group)).collect()
    }

    /// Moves a tab in front of `before` (or to the end) and into `group`.
    fn move_tab(&mut self, id: u64, before: Option<u64>, group: Option<u64>) {
        if before == Some(id) || group.is_some_and(|group| self.group(group).is_none()) {
            return;
        }
        let Some(from) = self.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        let mut tab = self.tabs.remove(from);
        let to = match before {
            Some(before) => match self.tabs.iter().position(|tab| tab.id == before) {
                Some(index) => index,
                None => {
                    self.tabs.insert(from, tab);
                    return;
                }
            },
            None => self.tabs.len(),
        };
        tab.group = group;
        self.tabs.insert(to, tab);
        self.normalize_groups();
    }

    fn new_group(&mut self, id: u64) {
        if self.tabs.iter().all(|tab| tab.id != id) {
            return;
        }
        let color = (0..GROUP_COLOR_COUNT)
            .find(|color| self.groups.iter().all(|group| group.color != *color))
            .unwrap_or((self.groups.len() % GROUP_COLOR_COUNT as usize) as u8);
        self.next_group_id += 1;
        let group = self.next_group_id;
        self.groups.push(TabGroup {
            id: group,
            name: String::new(),
            color,
            collapsed: false,
        });
        self.assign_group(id, Some(group));
    }

    /// Puts a tab at the end of `group`, or just after its old group when leaving it.
    fn assign_group(&mut self, id: u64, group: Option<u64>) {
        let Some(from) = self.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        let anchor = group.or(self.tabs[from].group);
        if group.is_some_and(|group| self.group(group).is_none()) || self.tabs[from].group == group
        {
            return;
        }
        let mut tab = self.tabs.remove(from);
        tab.group = group;
        let to = anchor
            .and_then(|anchor| self.tabs.iter().rposition(|tab| tab.group == Some(anchor)))
            .map(|index| index + 1)
            .unwrap_or(from);
        self.tabs.insert(to, tab);
        self.normalize_groups();
    }

    /// Keeps each group's tabs contiguous, at the position of its first tab.
    fn normalize_groups(&mut self) {
        let mut ordered = Vec::with_capacity(self.tabs.len());
        let mut placed = Vec::new();
        for tab in &self.tabs {
            match tab.group {
                None => ordered.push(tab.clone()),
                Some(group) if !placed.contains(&group) => {
                    placed.push(group);
                    ordered.extend(
                        self.tabs
                            .iter()
                            .filter(|tab| tab.group == Some(group))
                            .cloned(),
                    );
                }
                Some(_) => {}
            }
        }
        self.tabs = ordered;
        self.prune_empty_groups();
    }

    fn prune_empty_groups(&mut self) {
        let tabs = &self.tabs;
        self.groups
            .retain(|group| tabs.iter().any(|tab| tab.group == Some(group.id)));
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let active = self
            .active_id
            .and_then(|id| self.tabs.iter().position(|tab| tab.id == id));
        let saved = PersistedSession {
            version: 2,
            active,
            tabs: self
                .tabs
                .iter()
                .map(|tab| PersistedTab {
                    path: tab.path.clone(),
                    group: tab.group,
                })
                .collect(),
            groups: self.groups.clone(),
        };
        let body = serde_json::to_vec_pretty(&saved).map_err(io::Error::other)?;
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, body)?;
        #[cfg(target_os = "windows")]
        if path.exists() {
            fs::remove_file(path)?;
        }
        fs::rename(temporary, path)
    }

    pub fn active(&self) -> Option<&DocumentTab> {
        let id = self.active_id?;
        self.tabs.iter().find(|tab| tab.id == id)
    }

    pub fn active_mut(&mut self) -> Option<&mut DocumentTab> {
        let id = self.active_id?;
        self.tabs.iter_mut().find(|tab| tab.id == id)
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut DocumentTab> {
        self.tabs.iter_mut().find(|tab| tab.id == id)
    }

    pub fn relocate(&mut self, id: u64, path: PathBuf) -> bool {
        let path = normalize_path(path);
        if self.tabs.iter().any(|tab| tab.id != id && tab.path == path) {
            return false;
        }
        let Some(tab) = self.get_mut(id) else {
            return false;
        };
        tab.path = path;
        tab.missing = !tab.path.exists();
        true
    }
}

fn normalize_path(path: PathBuf) -> PathBuf {
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    fs::canonicalize(&absolute).unwrap_or_else(|_| {
        absolute
            .parent()
            .and_then(|parent| fs::canonicalize(parent).ok())
            .and_then(|parent| absolute.file_name().map(|name| parent.join(name)))
            .unwrap_or(absolute)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "md-preview-session-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn opening_the_same_path_activates_without_duplicate() {
        let dir = temp_dir("dedupe");
        let file = dir.join("note.md");
        fs::write(&file, "# Note").unwrap();
        let mut session = DocumentSession::default();

        let first = session.open(file.clone(), false);
        let second = session.open(file.clone(), true);

        assert_eq!(first, second);
        assert_eq!(session.tabs.len(), 1);
        assert_eq!(session.active_id, Some(first));
        assert!(session.tabs[0].edit_on_open);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn closing_active_tab_selects_the_next_neighbor() {
        let dir = temp_dir("close");
        let mut session = DocumentSession::default();
        let first = session.open(dir.join("one.md"), false);
        let second = session.open(dir.join("two.md"), false);
        let third = session.open(dir.join("three.md"), false);
        assert!(session.activate(second));

        assert!(session.close(second));

        assert_eq!(session.active_id, Some(third));
        assert_eq!(
            session.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
            vec![first, third]
        );
        let _ = fs::remove_dir_all(dir);
    }

    fn tab_ids(session: &DocumentSession) -> Vec<u64> {
        session.tabs.iter().map(|tab| tab.id).collect()
    }

    fn mv(id: u64, before: Option<u64>, group: Option<u64>) -> TabLayoutChange {
        TabLayoutChange::Move { id, before, group }
    }

    fn groups_of(session: &DocumentSession) -> Vec<Option<u64>> {
        session.tabs.iter().map(|tab| tab.group).collect()
    }

    fn open_many(session: &mut DocumentSession, dir: &Path, names: &[&str]) -> Vec<u64> {
        names
            .iter()
            .map(|name| session.open(dir.join(name), false))
            .collect()
    }

    #[test]
    fn moving_a_tab_reorders_without_changing_the_active_tab() {
        let dir = temp_dir("move");
        let mut session = DocumentSession::default();
        let [a, b, c] = open_many(&mut session, &dir, &["a.md", "b.md", "c.md"])[..] else {
            unreachable!()
        };

        assert!(session.apply_layout(mv(c, Some(a), None)));
        assert_eq!(tab_ids(&session), vec![c, a, b]);
        assert!(session.apply_layout(mv(c, None, None)));
        assert_eq!(tab_ids(&session), vec![a, b, c]);
        assert!(session.apply_layout(mv(a, Some(c), None)));
        assert_eq!(tab_ids(&session), vec![b, a, c]);
        assert_eq!(session.active_id, Some(c));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn moving_to_an_unknown_or_same_position_is_rejected() {
        let dir = temp_dir("move-reject");
        let mut session = DocumentSession::default();
        let [a, b] = open_many(&mut session, &dir, &["a.md", "b.md"])[..] else {
            unreachable!()
        };

        assert!(!session.apply_layout(mv(a, Some(a), None)));
        assert!(!session.apply_layout(mv(a, Some(999), None)));
        assert!(!session.apply_layout(mv(a, Some(b), None)));
        assert!(!session.apply_layout(mv(b, None, None)));
        assert!(!session.apply_layout(mv(999, None, None)));
        assert!(!session.apply_layout(mv(a, None, Some(42))));
        assert_eq!(tab_ids(&session), vec![a, b]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn new_groups_take_distinct_colors_and_leave_a_group_contiguous() {
        let dir = temp_dir("new-group");
        let mut session = DocumentSession::default();
        let [a, b, c, d] = open_many(&mut session, &dir, &["a.md", "b.md", "c.md", "d.md"])[..]
        else {
            unreachable!()
        };

        assert!(session.apply_layout(TabLayoutChange::NewGroup(a)));
        let g1 = session.tabs[0].group.unwrap();
        assert!(session.apply_layout(TabLayoutChange::AssignGroup(c, Some(g1))));
        // c joins at the end of g1, so g1 stays one block: a, c, b, d.
        assert_eq!(tab_ids(&session), vec![a, c, b, d]);

        // A tab split off the front of a group stays in front of it.
        assert!(session.apply_layout(TabLayoutChange::NewGroup(a)));
        let g2 = session.tabs[0].group.unwrap();
        assert_eq!(tab_ids(&session), vec![a, c, b, d]);
        assert_eq!(groups_of(&session), vec![Some(g2), Some(g1), None, None]);

        // A tab split off the middle of a group lands just after it.
        session.apply_layout(TabLayoutChange::AssignGroup(b, Some(g1)));
        session.apply_layout(TabLayoutChange::AssignGroup(d, Some(g1)));
        assert_eq!(tab_ids(&session), vec![a, c, b, d]);
        assert!(session.apply_layout(TabLayoutChange::NewGroup(b)));
        let g3 = session.tabs[3].group.unwrap();
        assert_eq!(tab_ids(&session), vec![a, c, d, b]);
        assert_eq!(
            groups_of(&session),
            vec![Some(g2), Some(g1), Some(g1), Some(g3)]
        );
        assert_ne!(
            session.group(g1).unwrap().color,
            session.group(g2).unwrap().color
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn leaving_or_emptying_a_group_prunes_it() {
        let dir = temp_dir("leave-group");
        let mut session = DocumentSession::default();
        let [a, b, c] = open_many(&mut session, &dir, &["a.md", "b.md", "c.md"])[..] else {
            unreachable!()
        };
        session.apply_layout(TabLayoutChange::NewGroup(a));
        let g = session.tabs[0].group.unwrap();
        session.apply_layout(TabLayoutChange::AssignGroup(b, Some(g)));

        assert!(session.apply_layout(TabLayoutChange::AssignGroup(a, None)));
        assert_eq!(tab_ids(&session), vec![b, a, c]);
        assert_eq!(groups_of(&session), vec![Some(g), None, None]);

        assert!(session.close(b));
        assert!(session.groups.is_empty());
        assert!(!session.apply_layout(TabLayoutChange::AssignGroup(a, Some(g))));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn dragging_into_and_out_of_a_group_keeps_blocks_contiguous() {
        let dir = temp_dir("drag-group");
        let mut session = DocumentSession::default();
        let [a, b, c, d] = open_many(&mut session, &dir, &["a.md", "b.md", "c.md", "d.md"])[..]
        else {
            unreachable!()
        };
        session.apply_layout(TabLayoutChange::NewGroup(a));
        let g = session.tabs[0].group.unwrap();
        session.apply_layout(TabLayoutChange::AssignGroup(b, Some(g)));

        // Drop d between a and b inside g.
        assert!(session.apply_layout(mv(d, Some(b), Some(g))));
        assert_eq!(tab_ids(&session), vec![a, d, b, c]);
        assert_eq!(groups_of(&session), vec![Some(g), Some(g), Some(g), None]);

        // An ungrouped tab dropped inside the block is pushed back out behind it.
        assert!(!session.apply_layout(mv(c, Some(d), None)));
        assert_eq!(tab_ids(&session), vec![a, d, b, c]);
        assert_eq!(groups_of(&session), vec![Some(g), Some(g), Some(g), None]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn group_rename_color_collapse_ungroup_and_close() {
        let dir = temp_dir("group-ops");
        let mut session = DocumentSession::default();
        let [a, b, c] = open_many(&mut session, &dir, &["a.md", "b.md", "c.md"])[..] else {
            unreachable!()
        };
        session.apply_layout(TabLayoutChange::NewGroup(a));
        let g = session.tabs[0].group.unwrap();
        session.apply_layout(TabLayoutChange::AssignGroup(b, Some(g)));

        assert!(session.apply_layout(TabLayoutChange::RenameGroup(g, "  Plans ".into())));
        assert!(session.apply_layout(TabLayoutChange::RecolorGroup(g, 5)));
        assert!(!session.apply_layout(TabLayoutChange::RecolorGroup(g, GROUP_COLOR_COUNT)));
        assert!(session.apply_layout(TabLayoutChange::ToggleGroup(g)));
        assert_eq!(
            session.group(g),
            Some(&TabGroup {
                id: g,
                name: "Plans".into(),
                color: 5,
                collapsed: true
            })
        );

        assert!(session.apply_layout(TabLayoutChange::Ungroup(g)));
        assert!(session.groups.is_empty());
        assert_eq!(groups_of(&session), vec![None, None, None]);

        session.apply_layout(TabLayoutChange::NewGroup(b));
        let g = session
            .tabs
            .iter()
            .find(|tab| tab.id == b)
            .unwrap()
            .group
            .unwrap();
        session.apply_layout(TabLayoutChange::AssignGroup(c, Some(g)));
        assert!(session.activate(b));
        assert!(session.close_group(g));
        assert_eq!(tab_ids(&session), vec![a]);
        assert_eq!(session.active_id, Some(a));
        assert!(!session.close_group(g));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn persisted_groups_round_trip_and_v1_sessions_still_load() {
        let dir = temp_dir("groups-roundtrip");
        let state_path = dir.join("session.json");
        let mut session = DocumentSession::default();
        let [a, b, _c] = open_many(&mut session, &dir, &["a.md", "b.md", "c.md"])[..] else {
            unreachable!()
        };
        session.apply_layout(TabLayoutChange::NewGroup(b));
        let g = session
            .tabs
            .iter()
            .find(|tab| tab.id == b)
            .unwrap()
            .group
            .unwrap();
        session.apply_layout(TabLayoutChange::RenameGroup(g, "Pinned".into()));
        session.apply_layout(TabLayoutChange::ToggleGroup(g));
        session.apply_layout(TabLayoutChange::AssignGroup(a, Some(g)));
        assert!(session.activate(a));
        session.save(&state_path).unwrap();

        let restored = DocumentSession::load(&state_path);
        let names = restored
            .tabs
            .iter()
            .map(|tab| tab.path.file_name().unwrap().to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["b.md", "a.md", "c.md"]);
        assert_eq!(restored.groups, session.groups);
        assert_eq!(groups_of(&restored), vec![Some(g), Some(g), None]);
        assert_eq!(
            restored.active().unwrap().path,
            dir_canonical(&dir).join("a.md")
        );

        // A new group after reload must not reuse a persisted id.
        let mut restored = restored;
        let c_id = restored.tabs[2].id;
        restored.apply_layout(TabLayoutChange::NewGroup(c_id));
        assert_ne!(restored.tabs[2].group, Some(g));

        fs::write(
            &state_path,
            format!(
                r#"{{"version":1,"active":0,"tabs":[{:?},{:?}]}}"#,
                dir.join("a.md"),
                dir.join("b.md")
            ),
        )
        .unwrap();
        let v1 = DocumentSession::load(&state_path);
        assert_eq!(v1.tabs.len(), 2);
        assert!(v1.groups.is_empty());
        assert_eq!(v1.active().unwrap().path, dir_canonical(&dir).join("a.md"));
        let _ = fs::remove_dir_all(dir);
    }

    fn dir_canonical(dir: &Path) -> PathBuf {
        fs::canonicalize(dir).unwrap()
    }

    #[test]
    fn persisted_session_keeps_missing_tabs_and_active_order() {
        let dir = temp_dir("roundtrip");
        let state_path = dir.join("session.json");
        let existing = dir.join("existing.md");
        let missing = dir.join("missing.md");
        fs::write(&existing, "# Existing").unwrap();
        let mut session = DocumentSession::default();
        session.open(existing.clone(), false);
        session.open(missing.clone(), false);

        session.save(&state_path).unwrap();
        let restored = DocumentSession::load(&state_path);

        assert_eq!(restored.tabs.len(), 2);
        assert_eq!(restored.tabs[0].path, fs::canonicalize(existing).unwrap());
        assert_eq!(
            restored.tabs[1].path,
            fs::canonicalize(&dir)
                .unwrap()
                .join(missing.file_name().unwrap())
        );
        assert!(!restored.tabs[0].missing);
        assert!(restored.tabs[1].missing);
        assert_eq!(restored.active_id, restored.tabs.get(1).map(|tab| tab.id));
        let _ = fs::remove_dir_all(dir);
    }
}
