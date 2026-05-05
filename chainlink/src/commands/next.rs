use anyhow::Result;
use serde::Serialize;
use std::path::Path;

use crate::db::Database;
use crate::lock_check::{self, LockStatus};
use crate::models::Issue;
use crate::utils::format_issue_id;

/// Progress tuple: (completed subissues, total subissues)
type Progress = Option<(i32, i32)>;

/// Scored issue with priority score and progress
type ScoredIssue = (Issue, i32, Progress);

#[derive(Clone)]
struct RecommendedIssue {
    issue: Issue,
    progress: Progress,
}

struct Recommendations {
    next: Option<RecommendedIssue>,
    also_ready: Vec<RecommendedIssue>,
}

#[derive(Serialize)]
struct NextJson {
    next: Option<RecommendedIssueJson>,
    also_ready: Vec<RecommendedIssueJson>,
}

#[derive(Serialize)]
struct RecommendedIssueJson {
    issue: Issue,
    progress: Option<ProgressJson>,
}

#[derive(Serialize)]
struct ProgressJson {
    completed_subissues: i32,
    total_subissues: i32,
}

/// Priority order for sorting (higher = more important)
fn priority_weight(priority: &str) -> i32 {
    match priority {
        "critical" => 4,
        "high" => 3,
        "medium" => 2,
        "low" => 1,
        _ => 0,
    }
}

/// Calculate progress for issues with subissues
fn calculate_progress(db: &Database, issue: &Issue) -> Result<Progress> {
    let subissues = db.get_subissues(issue.id)?;
    if subissues.is_empty() {
        return Ok(None);
    }

    let total = subissues.len() as i32;
    let closed = subissues.iter().filter(|s| s.status == "closed").count() as i32;
    Ok(Some((closed, total)))
}

fn is_locked_by_other(chainlink_dir: &Path, issue_id: i64) -> bool {
    matches!(
        lock_check::check_lock(chainlink_dir, issue_id),
        Ok(LockStatus::LockedByOther { stale: false, .. })
    )
}

fn recommendations(db: &Database, chainlink_dir: &Path) -> Result<Recommendations> {
    let ready = db.list_ready_issues()?;

    if ready.is_empty() {
        return Ok(Recommendations {
            next: None,
            also_ready: Vec::new(),
        });
    }

    // Score and sort issues
    let mut scored: Vec<ScoredIssue> = Vec::new();

    for issue in &ready {
        // Skip subissues - we want to recommend parent issues or standalone issues
        if issue.parent_id.is_some() {
            continue;
        }

        // Best-effort: skip issues locked by other agents
        if is_locked_by_other(chainlink_dir, issue.id) {
            continue;
        }

        let priority_score = priority_weight(&issue.priority) * 100;
        let progress = calculate_progress(db, issue)?;

        // Boost score for issues that are partially complete (finish what you started)
        let progress_bonus = match &progress {
            Some((closed, total)) if *closed > 0 && *closed < *total => 50,
            _ => 0,
        };

        let score = priority_score + progress_bonus;
        scored.push((issue.clone(), score, progress));
    }

    // Sort by score descending
    scored.sort_by(|a, b| b.1.cmp(&a.1));

    if scored.is_empty() {
        // All ready issues are subissues, show them instead
        let mut subissues = ready
            .into_iter()
            .filter(|issue| issue.parent_id.is_some())
            .filter(|issue| !is_locked_by_other(chainlink_dir, issue.id))
            .map(|issue| RecommendedIssue {
                issue,
                progress: None,
            });

        return Ok(Recommendations {
            next: subissues.next(),
            also_ready: subissues.take(3).collect(),
        });
    }

    let mut recommended = scored
        .into_iter()
        .map(|(issue, _score, progress)| RecommendedIssue { issue, progress });

    Ok(Recommendations {
        next: recommended.next(),
        also_ready: recommended.take(3).collect(),
    })
}

pub fn run(db: &Database, chainlink_dir: &Path) -> Result<()> {
    let recommendations = recommendations(db, chainlink_dir)?;

    let Some(top) = &recommendations.next else {
        println!("No issues ready to work on.");
        println!(
            "Use 'chainlink list' to see all issues or 'chainlink blocked' to see blocked issues."
        );
        return Ok(());
    };

    if top.issue.parent_id.is_some() {
        println!(
            "Next: {} [{}] {}",
            format_issue_id(top.issue.id),
            top.issue.priority,
            top.issue.title
        );
        if let Some(parent_id) = top.issue.parent_id {
            println!("       (subissue of {})", format_issue_id(parent_id));
        }
        return Ok(());
    }

    // Recommend the top issue
    println!(
        "Next: {} [{}] {}",
        format_issue_id(top.issue.id),
        top.issue.priority,
        top.issue.title
    );

    if let Some((closed, total)) = top.progress {
        println!("       Progress: {}/{} subissues complete", closed, total);
    }

    if let Some(desc) = &top.issue.description {
        if !desc.is_empty() {
            let preview: String = desc.chars().take(80).collect();
            let suffix = if desc.chars().count() > 80 { "..." } else { "" };
            println!("       {}{}", preview, suffix);
        }
    }

    println!();
    println!("Run: chainlink session work {}", top.issue.id);

    // Show runners-up if any
    if !recommendations.also_ready.is_empty() {
        println!();
        println!("Also ready:");
        for issue in &recommendations.also_ready {
            let progress_str = match issue.progress {
                Some((c, t)) => format!(" ({}/{})", c, t),
                None => String::new(),
            };
            println!(
                "  {} [{}] {}{}",
                format_issue_id(issue.issue.id),
                issue.issue.priority,
                issue.issue.title,
                progress_str
            );
        }
    }

    Ok(())
}

fn progress_json(progress: Progress) -> Option<ProgressJson> {
    progress.map(|(completed_subissues, total_subissues)| ProgressJson {
        completed_subissues,
        total_subissues,
    })
}

fn recommended_issue_json(recommended: RecommendedIssue) -> RecommendedIssueJson {
    RecommendedIssueJson {
        issue: recommended.issue,
        progress: progress_json(recommended.progress),
    }
}

pub fn run_json(db: &Database, chainlink_dir: &Path) -> Result<()> {
    let recommendations = recommendations(db, chainlink_dir)?;
    let output = NextJson {
        next: recommendations.next.map(recommended_issue_json),
        also_ready: recommendations
            .also_ready
            .into_iter()
            .map(recommended_issue_json)
            .collect(),
    };

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use tempfile::tempdir;

    fn setup_test_db() -> (Database, std::path::PathBuf, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).unwrap();
        let chainlink_dir = dir.path().join(".chainlink");
        std::fs::create_dir_all(&chainlink_dir).unwrap();
        (db, chainlink_dir, dir)
    }

    #[test]
    fn test_priority_weight_critical() {
        assert_eq!(priority_weight("critical"), 4);
    }

    #[test]
    fn test_priority_weight_high() {
        assert_eq!(priority_weight("high"), 3);
    }

    #[test]
    fn test_priority_weight_medium() {
        assert_eq!(priority_weight("medium"), 2);
    }

    #[test]
    fn test_priority_weight_low() {
        assert_eq!(priority_weight("low"), 1);
    }

    #[test]
    fn test_priority_weight_unknown() {
        assert_eq!(priority_weight("unknown"), 0);
    }

    #[test]
    fn test_run_no_issues() {
        let (db, cl, _dir) = setup_test_db();
        run(&db, &cl).unwrap();
        let ready = db.list_ready_issues().unwrap();
        assert!(ready.is_empty());
    }

    #[test]
    fn test_run_with_issues() {
        let (db, cl, _dir) = setup_test_db();
        let id = db.create_issue("Issue 1", None, "high").unwrap();

        run(&db, &cl).unwrap();
        let ready = db.list_ready_issues().unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, id);
    }

    #[test]
    fn test_run_prioritizes_higher() {
        let (db, cl, _dir) = setup_test_db();
        db.create_issue("Low priority", None, "low").unwrap();
        let critical_id = db
            .create_issue("Critical priority", None, "critical")
            .unwrap();
        db.create_issue("Medium priority", None, "medium").unwrap();

        run(&db, &cl).unwrap();
        let ready = db.list_ready_issues().unwrap();
        assert_eq!(ready.len(), 3);
        let critical = ready.iter().find(|i| i.id == critical_id).unwrap();
        assert_eq!(critical.priority, "critical");
        assert_eq!(priority_weight("critical"), 4);
        assert!(priority_weight("critical") > priority_weight("low"));
        assert!(priority_weight("critical") > priority_weight("medium"));
    }

    #[test]
    fn test_calculate_progress_no_subissues() {
        let (db, _cl, _dir) = setup_test_db();
        let id = db.create_issue("Simple issue", None, "medium").unwrap();
        let issue = db.get_issue(id).unwrap().unwrap();

        let progress = calculate_progress(&db, &issue).unwrap();
        assert!(progress.is_none());
    }

    #[test]
    fn test_calculate_progress_with_subissues() {
        let (db, _cl, _dir) = setup_test_db();
        let parent_id = db.create_issue("Parent", None, "high").unwrap();
        let child1 = db
            .create_subissue(parent_id, "Child 1", None, "medium")
            .unwrap();
        db.create_subissue(parent_id, "Child 2", None, "medium")
            .unwrap();
        db.close_issue(child1).unwrap();

        let issue = db.get_issue(parent_id).unwrap().unwrap();
        let progress = calculate_progress(&db, &issue).unwrap();

        assert!(progress.is_some());
        let (closed, total) = progress.unwrap();
        assert_eq!(closed, 1);
        assert_eq!(total, 2);
    }

    #[test]
    fn test_run_skips_blocked() {
        let (db, cl, _dir) = setup_test_db();
        let blocker = db.create_issue("Blocker", None, "high").unwrap();
        let blocked = db.create_issue("Blocked", None, "critical").unwrap();
        db.add_dependency(blocked, blocker).unwrap();

        run(&db, &cl).unwrap();
        let ready = db.list_ready_issues().unwrap();
        assert!(
            !ready.iter().any(|i| i.id == blocked),
            "Blocked issue should not be in ready list"
        );
        assert!(
            ready.iter().any(|i| i.id == blocker),
            "Blocker should be in ready list"
        );
    }

    #[test]
    fn test_run_all_issues_closed() {
        let (db, cl, _dir) = setup_test_db();
        let id = db.create_issue("Done", None, "medium").unwrap();
        db.close_issue(id).unwrap();

        run(&db, &cl).unwrap();
        let ready = db.list_ready_issues().unwrap();
        assert!(
            ready.is_empty(),
            "Closed issues should not appear in ready list"
        );
    }

    proptest! {
        #[test]
        fn prop_priority_weight_valid(priority in "low|medium|high|critical") {
            let weight = priority_weight(&priority);
            prop_assert!((1..=4).contains(&weight));
        }

        #[test]
        fn prop_run_never_panics(count in 0usize..5) {
            let (db, cl, _dir) = setup_test_db();
            for i in 0..count {
                db.create_issue(&format!("Issue {}", i), None, "medium").unwrap();
            }
            let result = run(&db, &cl);
            prop_assert!(result.is_ok());
        }
    }
}
