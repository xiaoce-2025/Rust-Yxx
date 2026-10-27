//! Ordered note filenames, with collision-free moves and crash recovery.
use super::lesson_storage::LessonInfo;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Component, Path},
    sync::Mutex,
};

pub(super) static STORAGE_LOCK: Mutex<()> = Mutex::new(());
const TRANSACTION: &str = ".lesson-order";

#[derive(Serialize, Deserialize)]
struct FileMove {
    source: String,
    target: Option<String>,
    position: usize,
}
#[derive(Serialize, Deserialize)]
struct Journal {
    publishing: bool,
    #[serde(default)]
    restoring: bool,
    #[serde(default)]
    committed: bool,
    previous_index: Option<String>,
    archive_id: String,
    index: String,
    moves: Vec<FileMove>,
}

pub(super) fn filename(course_id: &str, number: usize) -> String {
    format!("{course_id}_lesson_{number}.tex")
}

pub(super) fn single_filename(name: &str) -> Result<(), String> {
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
        || name.contains(['/', '\\'])
    {
        return Err("笔记文件名无效".into());
    }
    Ok(())
}

fn cleanup(dir: &Path, journal: &Journal) -> Result<(), String> {
    let transaction = dir.join(TRANSACTION);
    for (index, movement) in journal.moves.iter().enumerate() {
        if movement.target.is_none() {
            let staged = transaction.join(format!("{index}.tex"));
            if staged.exists() {
                let archive = dir.join(".removed-notes").join(&journal.archive_id);
                fs::create_dir_all(&archive).map_err(|e| e.to_string())?;
                fs::rename(staged, archive.join(&movement.source)).map_err(|e| e.to_string())?;
            }
        }
    }
    fs::remove_dir_all(transaction).map_err(|e| e.to_string())
}

// Called under STORAGE_LOCK, including when loading after a server restart.
pub(super) fn recover(dir: &Path) -> Result<(), String> {
    let transaction = dir.join(TRANSACTION);
    if !transaction.exists() {
        return Ok(());
    }
    let journal_path = transaction.join("journal.json");
    if !journal_path.exists() {
        // A crash before the journal is written cannot have moved any note.
        fs::remove_dir_all(transaction).map_err(|e| e.to_string())?;
        return Ok(());
    }
    let mut journal: Journal =
        serde_json::from_slice(&fs::read(&journal_path).map_err(|e| e.to_string())?)
            .map_err(|e| format!("课节编号恢复记录损坏: {e}"))?;
    single_filename(&journal.archive_id)?;
    for movement in &journal.moves {
        single_filename(&movement.source)?;
        if let Some(target) = &movement.target {
            single_filename(target)?;
        }
    }
    if journal.committed
        || (journal.previous_index.as_deref() != Some(journal.index.as_str())
            && fs::read(dir.join("index.json")).ok().as_deref() == Some(journal.index.as_bytes()))
    {
        return cleanup(dir, &journal);
    }
    if journal.publishing && !journal.restoring {
        // First vacate every published destination, then restore the old names.
        for (index, movement) in journal.moves.iter().enumerate().rev() {
            let staged = transaction.join(format!("{index}.tex"));
            if !staged.exists() {
                let target = movement.target.as_ref().ok_or("恢复课节笔记时缺少原文件")?;
                fs::rename(dir.join(target), staged)
                    .map_err(|e| format!("恢复课节笔记失败: {e}"))?;
            }
        }
        journal.restoring = true;
        super::lesson_storage::atomic_write(
            &journal_path,
            &serde_json::to_vec(&journal).map_err(|e| e.to_string())?,
        )?;
    }
    for (index, movement) in journal.moves.iter().enumerate().rev() {
        let staged = transaction.join(format!("{index}.tex"));
        if staged.exists() {
            if dir.join(&movement.source).exists() {
                return Err("恢复课节笔记时原文件名被占用".into());
            }
            fs::rename(staged, dir.join(&movement.source))
                .map_err(|e| format!("恢复课节笔记失败: {e}"))?;
        }
    }
    fs::remove_dir_all(transaction).map_err(|e| e.to_string())
}

pub(super) fn reconcile(
    dir: &Path,
    course_id: &str,
    old: &[LessonInfo],
    new: &mut Vec<LessonInfo>,
) -> Result<(), String> {
    let _guard = STORAGE_LOCK.lock().map_err(|_| "课节存储锁异常")?;
    single_filename(course_id)?;
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    recover(dir)?;
    let mut ordered = new.clone();
    super::lesson_parser::sort_lessons(&mut ordered);
    let mut identities = HashSet::new();
    if ordered.iter().any(|l| !identities.insert(l.id.clone())) {
        return Err("课节身份重复，已停止重编号".into());
    }
    identities.clear();
    if old.iter().any(|l| !identities.insert(l.id.clone())) {
        return Err("已有课节身份重复，已停止重编号".into());
    }
    let old_positions: HashMap<_, _> = old
        .iter()
        .enumerate()
        .map(|(i, l)| (l.id.as_str(), i))
        .collect();
    let mut files: HashMap<String, FileMove> = HashMap::new();
    let mut owners: HashMap<String, String> = HashMap::new();
    for (index, lesson) in old.iter().enumerate() {
        if let Some(source) = &lesson.tex_filename {
            single_filename(source)?;
            if owners.insert(source.clone(), lesson.id.clone()).is_some() {
                return Err("多个课节指向同一笔记文件，已停止重编号".into());
            }
            files.insert(
                source.clone(),
                FileMove {
                    source: source.clone(),
                    target: None,
                    position: index,
                },
            );
        }
    }
    for (index, lesson) in ordered.iter_mut().enumerate() {
        if let Some(source) = lesson.tex_filename.clone() {
            single_filename(&source)?;
            if owners.get(&source).is_some_and(|owner| owner != &lesson.id) {
                return Err("笔记文件归属冲突，已停止重编号".into());
            }
            owners.insert(source.clone(), lesson.id.clone());
            let target = filename(course_id, index + 1);
            single_filename(&target)?;
            files.insert(
                source.clone(),
                FileMove {
                    source,
                    target: Some(target.clone()),
                    position: old_positions
                        .get(lesson.id.as_str())
                        .copied()
                        .unwrap_or(index),
                },
            );
            lesson.tex_filename = Some(target);
        }
    }
    // A missing note being explicitly retired needs no file move. Missing
    // retained notes still abort, since their content cannot be reconstructed.
    files.retain(|_, movement| movement.target.is_some() || dir.join(&movement.source).exists());
    for movement in files.values() {
        if !dir.join(&movement.source).is_file() {
            return Err(format!("课节笔记不存在: {}，已停止更新", movement.source));
        }
        if let Some(target) = &movement.target {
            if dir.join(target).exists() && !files.contains_key(target) {
                return Err(format!("笔记文件名被未关联文件占用: {target}，已停止更新"));
            }
        }
    }
    let index = serde_json::to_string_pretty(&ordered).map_err(|e| e.to_string())?;
    let mut moves: Vec<_> = files
        .into_values()
        .filter(|m| m.target.as_deref() != Some(m.source.as_str()))
        .collect();
    moves.sort_by(|a, b| a.position.cmp(&b.position).then(a.source.cmp(&b.source)));
    // Insertions: from the last existing lesson backwards. Removals: forwards.
    if ordered.len() > old.len() {
        moves.reverse();
    }
    if moves.is_empty() {
        super::lesson_storage::atomic_write(&dir.join("index.json"), index.as_bytes())?;
        *new = ordered;
        return Ok(());
    }
    let transaction = dir.join(TRANSACTION);
    fs::create_dir(&transaction).map_err(|e| e.to_string())?;
    let previous_index = fs::read_to_string(dir.join("index.json")).ok();
    let mut journal = Journal {
        publishing: false,
        restoring: false,
        committed: false,
        previous_index,
        archive_id: uuid::Uuid::new_v4().simple().to_string(),
        index,
        moves,
    };
    let result = (|| -> Result<(), String> {
        super::lesson_storage::atomic_write(
            &transaction.join("journal.json"),
            &serde_json::to_vec(&journal).map_err(|e| e.to_string())?,
        )?;
        for (index, movement) in journal.moves.iter().enumerate() {
            fs::rename(
                dir.join(&movement.source),
                transaction.join(format!("{index}.tex")),
            )
            .map_err(|e| format!("暂存课节笔记失败: {e}"))?;
        }
        journal.publishing = true;
        super::lesson_storage::atomic_write(
            &transaction.join("journal.json"),
            &serde_json::to_vec(&journal).map_err(|e| e.to_string())?,
        )?;
        for (index, movement) in journal.moves.iter().enumerate() {
            if let Some(target) = &movement.target {
                if dir.join(target).exists() {
                    return Err(format!("课节笔记目标文件名被占用: {target}"));
                }
                fs::rename(transaction.join(format!("{index}.tex")), dir.join(target))
                    .map_err(|e| format!("重命名课节笔记失败: {e}"))?;
            }
        }
        super::lesson_storage::atomic_write(&dir.join("index.json"), journal.index.as_bytes())?;
        journal.committed = true;
        super::lesson_storage::atomic_write(
            &transaction.join("journal.json"),
            &serde_json::to_vec(&journal).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    })();
    if let Err(error) = result {
        return match recover(dir) {
            Ok(()) => Err(error),
            Err(recovery) => Err(format!("{error}；{recovery}，请保留恢复记录")),
        };
    }
    *new = ordered;
    // Index commit is complete; retained notes are now consistent. Cleanup is
    // recoverable on the next load even if archival fails (e.g. antivirus lock).
    if let Err(error) = cleanup(dir, &journal) {
        tracing::warn!("课节笔记已更新，待清理恢复记录: {}", error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lesson(id: &str, day: u32, processed: bool) -> LessonInfo {
        LessonInfo {
            enc_key: None,
            id: id.into(),
            video_url: String::new(),
            play_url: String::new(),
            date: format!("2026-01-{day:02}"),
            time: "08:00".into(),
            day_of_week: String::new(),
            is_processed: processed,
            tex_filename: None,
            manual_replay: None,
        }
    }
    fn setup(count: usize) -> (tempdir::TempDir, Vec<LessonInfo>) {
        let temp = tempdir::TempDir::new_in("target", "lesson-order").unwrap();
        let mut lessons = Vec::new();
        for i in 0..count {
            let mut l = lesson(&format!("id{}", i + 1), i as u32 + 1, true);
            let name = filename("c", i + 1);
            fs::write(temp.path().join(&name), &l.id).unwrap();
            l.tex_filename = Some(name);
            lessons.push(l);
        }
        fs::write(
            temp.path().join("index.json"),
            serde_json::to_vec_pretty(&lessons).unwrap(),
        )
        .unwrap();
        (temp, lessons)
    }
    fn verify(dir: &Path, lessons: &[LessonInfo]) {
        for (i, l) in lessons.iter().enumerate() {
            if let Some(name) = &l.tex_filename {
                assert_eq!(name, &filename("c", i + 1));
                assert_eq!(fs::read_to_string(dir.join(name)).unwrap(), l.id);
                assert!(l.is_processed);
            }
        }
        let saved: Vec<LessonInfo> =
            serde_json::from_slice(&fs::read(dir.join("index.json")).unwrap()).unwrap();
        assert_eq!(
            serde_json::to_value(saved).unwrap(),
            serde_json::to_value(lessons).unwrap()
        );
    }
    #[test]
    fn inserted_lessons_shift_existing_notes_backwards_without_overwriting() {
        let (temp, old) = setup(3);
        let mut new = old.clone();
        let mut inserted = lesson("new", 1, false);
        inserted.date = "2025-12-31".into();
        new.push(inserted);
        reconcile(temp.path(), "c", &old, &mut new).unwrap();
        assert_eq!(
            new.iter().map(|l| l.id.as_str()).collect::<Vec<_>>(),
            ["new", "id1", "id2", "id3"]
        );
        verify(temp.path(), &new);
        assert!(!temp.path().join(filename("c", 1)).exists());
        // Repeating an identical refresh must not move or duplicate any file.
        let old = new.clone();
        reconcile(temp.path(), "c", &old, &mut new).unwrap();
        verify(temp.path(), &new);
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 4);
    }
    #[test]
    fn removed_lessons_shift_notes_forwards_and_preserve_removed_content() {
        let (temp, old) = setup(4);
        let mut new = vec![old[1].clone(), old[3].clone()];
        reconcile(temp.path(), "c", &old, &mut new).unwrap();
        verify(temp.path(), &new);
        assert_eq!(new[0].id, "id2");
        assert_eq!(new[1].id, "id4");
        assert!(!temp.path().join(filename("c", 3)).exists());
        assert!(!temp.path().join(filename("c", 4)).exists());
        let archive = fs::read_dir(temp.path().join(".removed-notes"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(
            fs::read_to_string(archive.join(filename("c", 1))).unwrap(),
            "id1"
        );
        assert_eq!(
            fs::read_to_string(archive.join(filename("c", 3))).unwrap(),
            "id3"
        );
    }
    #[test]
    fn same_count_reordering_and_legacy_names_are_collision_free() {
        let (temp, old) = setup(3);
        let mut new = old.clone();
        new[2].date = "2025-12-31".into();
        reconcile(temp.path(), "c", &old, &mut new).unwrap();
        verify(temp.path(), &new);
        assert_eq!(
            new.iter().map(|l| l.id.as_str()).collect::<Vec<_>>(),
            ["id3", "id1", "id2"]
        );
        let mut legacy = new.clone();
        let old_name = legacy[1].tex_filename.clone().unwrap();
        let legacy_name = "c_lesson_app_uuid.tex";
        fs::rename(temp.path().join(old_name), temp.path().join(legacy_name)).unwrap();
        legacy[1].tex_filename = Some(legacy_name.into());
        let mut repaired = legacy.clone();
        reconcile(temp.path(), "c", &legacy, &mut repaired).unwrap();
        verify(temp.path(), &repaired);
    }
    #[test]
    fn unknown_destination_is_not_overwritten() {
        let (temp, old) = setup(1);
        fs::write(temp.path().join(filename("c", 2)), "unrelated").unwrap();
        let mut new = vec![lesson("new", 1, false), old[0].clone()];
        new[0].date = "2025-12-31".into();
        let before = fs::read(temp.path().join("index.json")).unwrap();
        assert!(reconcile(temp.path(), "c", &old, &mut new).is_err());
        assert_eq!(fs::read(temp.path().join("index.json")).unwrap(), before);
        assert_eq!(
            fs::read_to_string(temp.path().join(filename("c", 1))).unwrap(),
            "id1"
        );
        assert_eq!(
            fs::read_to_string(temp.path().join(filename("c", 2))).unwrap(),
            "unrelated"
        );
    }
    fn interrupted_fixture(restoring: bool) -> (tempdir::TempDir, Vec<LessonInfo>) {
        let (temp, old) = setup(2);
        let previous = fs::read_to_string(temp.path().join("index.json")).unwrap();
        let tx = temp.path().join(TRANSACTION);
        fs::create_dir(&tx).unwrap();
        let mut new = old.clone();
        for (i, l) in new.iter_mut().enumerate() {
            l.tex_filename = Some(filename("c", i + 2));
        }
        new.insert(0, lesson("new", 1, false));
        let journal = Journal {
            publishing: true,
            restoring,
            committed: false,
            previous_index: Some(previous),
            archive_id: uuid::Uuid::new_v4().simple().to_string(),
            index: serde_json::to_string_pretty(&new).unwrap(),
            moves: vec![
                FileMove {
                    source: filename("c", 1),
                    target: Some(filename("c", 2)),
                    position: 0,
                },
                FileMove {
                    source: filename("c", 2),
                    target: Some(filename("c", 3)),
                    position: 1,
                },
            ],
        };
        fs::write(
            tx.join("journal.json"),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        fs::rename(temp.path().join(filename("c", 1)), tx.join("0.tex")).unwrap();
        if !restoring {
            fs::rename(temp.path().join(filename("c", 2)), tx.join("1.tex")).unwrap();
            fs::rename(tx.join("0.tex"), temp.path().join(filename("c", 2))).unwrap();
        }
        (temp, old)
    }
    #[test]
    fn interrupted_publishing_and_interrupted_rollback_restore_original_files() {
        let _guard = STORAGE_LOCK.lock().unwrap();
        for restoring in [false, true] {
            let (temp, old) = interrupted_fixture(restoring);
            recover(temp.path()).unwrap();
            verify(temp.path(), &old);
            assert!(!temp.path().join(TRANSACTION).exists());
        }
    }
    #[test]
    fn committed_index_finishes_cleanup_instead_of_rolling_back() {
        let _guard = STORAGE_LOCK.lock().unwrap();
        let (temp, _old) = interrupted_fixture(false);
        let tx = temp.path().join(TRANSACTION);
        let journal: Journal =
            serde_json::from_slice(&fs::read(tx.join("journal.json")).unwrap()).unwrap();
        fs::rename(tx.join("1.tex"), temp.path().join(filename("c", 3))).unwrap();
        fs::write(temp.path().join("index.json"), &journal.index).unwrap();
        recover(temp.path()).unwrap();
        let new: Vec<LessonInfo> = serde_json::from_str(&journal.index).unwrap();
        verify(temp.path(), &new);
        assert!(!tx.exists());
    }
}
