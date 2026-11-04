//! SQLite store (rusqlite, bundled): projects/assets/clusters/
//! asset_clusters/people/plans/jobs per PLAN Sec 15.
//!
//! Single-writer model: one [`Db`] handle, WAL mode, idempotent
//! `CREATE ... IF NOT EXISTS` schema. One row per Live Photo pair in `assets`.

use crate::types::{Asset, AssetKind, Cluster, Job, JobState, PairingConfidence, Plan};
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::path::{Path, PathBuf};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS projects (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    folder TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    cloud_consent INTEGER NOT NULL DEFAULT 0,
    book_preset TEXT,
    music_pick TEXT
);
CREATE TABLE IF NOT EXISTS plan_edits (
    project_id TEXT PRIMARY KEY REFERENCES projects(id),
    edits_json TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS assets (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id),
    path TEXT NOT NULL,
    companion_path TEXT,
    kind TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    content_hash TEXT,
    phash INTEGER,
    width INTEGER,
    height INTEGER,
    duration_secs REAL,
    taken_at INTEGER,
    tz_offset INTEGER,
    time_guessed INTEGER NOT NULL DEFAULT 0,
    lat REAL,
    lon REAL,
    rotation_deg INTEGER NOT NULL DEFAULT 0,
    pair_confidence TEXT,
    user_caption TEXT,
    thumb_path TEXT,
    proxy_path TEXT,
    score_json TEXT
);
CREATE INDEX IF NOT EXISTS idx_assets_project ON assets(project_id);
CREATE TABLE IF NOT EXISTS clusters (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id),
    kind TEXT NOT NULL,
    label TEXT NOT NULL,
    start_time INTEGER,
    end_time INTEGER,
    lat REAL,
    lon REAL
);
CREATE TABLE IF NOT EXISTS asset_clusters (
    asset_id TEXT NOT NULL REFERENCES assets(id),
    cluster_id TEXT NOT NULL REFERENCES clusters(id),
    is_hero INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (asset_id, cluster_id)
);
CREATE TABLE IF NOT EXISTS people (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id),
    display_name TEXT,
    face_count INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS plans (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id),
    plan_json TEXT NOT NULL,
    plan_version INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    source TEXT NOT NULL DEFAULT 'local'
);
CREATE INDEX IF NOT EXISTS idx_plans_project ON plans(project_id, plan_version);
CREATE TABLE IF NOT EXISTS jobs (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id),
    kind TEXT NOT NULL,
    state TEXT NOT NULL,
    progress REAL NOT NULL DEFAULT 0,
    error TEXT,
    current_step TEXT,
    temp_dir TEXT,
    output_paths TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
";

/// Project row (mirrors the `projects` table).
#[derive(Debug, Clone)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub folder: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub cloud_consent: bool,
    pub book_preset: Option<String>,
    pub music_pick: Option<String>,
}

/// Single-writer handle over the TripTape SQLite store.
pub struct Db {
    conn: Connection,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

const JOB_COLS: &str = "id,kind,state,progress,error,current_step,temp_dir";

fn job_from_row(r: &Row) -> rusqlite::Result<Job> {
    Ok(Job {
        id: r.get(0)?,
        kind: r.get(1)?,
        state: job_state_from_str(&r.get::<_, String>(2)?),
        progress: r.get(3)?,
        error: r.get(4)?,
        current_step: r.get(5)?,
        temp_dir: r.get(6)?,
    })
}

pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

/// A project's latest plan before and after its Story-editor edits, with the
/// edits and assets it was resolved from.
pub struct EditedPlan {
    pub project_id: String,
    pub base: Plan,
    pub edited: Plan,
    pub edits: crate::plan::PlanEdits,
    pub assets: Vec<Asset>,
}

impl Db {
    /// Open (creating parent dirs) and apply the schema.
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
        }
        Self::init(Connection::open(path).map_err(|e| e.to_string())?)
    }

    /// Throwaway in-memory store (tests, dry runs).
    pub fn open_in_memory() -> Result<Self, String> {
        Self::init(Connection::open_in_memory().map_err(|e| e.to_string())?)
    }

    fn init(conn: Connection) -> Result<Self, String> {
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| e.to_string())?;
        conn.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
        // Migration for DBs created before capture timezones were stored.
        let has_tz_offset = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('assets') WHERE name='tz_offset'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map_err(|e| e.to_string())?
            > 0;
        if !has_tz_offset {
            conn.execute("ALTER TABLE assets ADD COLUMN tz_offset INTEGER", [])
                .map_err(|e| e.to_string())?;
        }
        // Migration for DBs created before embedded user captions were read.
        let has_caption = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('assets') WHERE name='user_caption'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map_err(|e| e.to_string())?
            > 0;
        if !has_caption {
            conn.execute("ALTER TABLE assets ADD COLUMN user_caption TEXT", [])
                .map_err(|e| e.to_string())?;
        }
        Ok(Self { conn })
    }

    /// Run `f` inside one transaction (one commit for batch writes);
    /// rolls back when `f` errs.
    pub fn in_transaction<T>(
        &self,
        f: impl FnOnce(&Self) -> Result<T, String>,
    ) -> Result<T, String> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| e.to_string())?;
        let out = f(self)?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(out)
    }

    // -- projects ---------------------------------------------------------
    pub fn create_project(&self, name: &str, folder: &str) -> Result<Project, String> {
        let t = now();
        let p = Project {
            id: new_id("prj"),
            name: name.to_string(),
            folder: folder.to_string(),
            created_at: t,
            updated_at: t,
            cloud_consent: false,
            book_preset: None,
            music_pick: None,
        };
        self.conn
            .execute(
                "INSERT INTO projects (id,name,folder,created_at,updated_at,cloud_consent) VALUES (?,?,?,?,?,0)",
                params![p.id, p.name, p.folder, p.created_at, p.updated_at],
            )
            .map_err(|e| e.to_string())?;
        Ok(p)
    }

    pub fn get_project(&self, id: &str) -> Result<Option<Project>, String> {
        self.conn
            .query_row(
                "SELECT id,name,folder,created_at,updated_at,cloud_consent,book_preset,music_pick FROM projects WHERE id=?1",
                params![id],
                |r| {
                    Ok(Project {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        folder: r.get(2)?,
                        created_at: r.get(3)?,
                        updated_at: r.get(4)?,
                        cloud_consent: r.get::<_, i64>(5)? != 0,
                        book_preset: r.get(6)?,
                        music_pick: r.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(|e| e.to_string())
    }

    // -- assets -----------------------------------------------------------
    pub fn upsert_asset(&self, project_id: &str, a: &Asset) -> Result<(), String> {
        let kind = match a.kind {
            AssetKind::Photo => "photo",
            AssetKind::Video => "video",
            AssetKind::LivePhoto => "livephoto",
        };
        let conf = match a.pairing_confidence {
            Some(PairingConfidence::ContentId) => Some("content-id"),
            Some(PairingConfidence::BasenameOnly) => Some("basename-only"),
            None => None,
        };
        self.conn.prepare_cached(
            "INSERT INTO assets (id,project_id,path,companion_path,kind,size_bytes,mtime,content_hash,phash,width,height,duration_secs,taken_at,tz_offset,time_guessed,lat,lon,rotation_deg,pair_confidence,user_caption)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
             ON CONFLICT(id) DO UPDATE SET project_id=excluded.project_id,path=excluded.path,companion_path=excluded.companion_path,kind=excluded.kind,size_bytes=excluded.size_bytes,mtime=excluded.mtime,content_hash=excluded.content_hash,phash=excluded.phash,width=excluded.width,height=excluded.height,duration_secs=excluded.duration_secs,taken_at=excluded.taken_at,tz_offset=excluded.tz_offset,time_guessed=excluded.time_guessed,lat=excluded.lat,lon=excluded.lon,rotation_deg=excluded.rotation_deg,pair_confidence=excluded.pair_confidence,user_caption=excluded.user_caption",
        ).and_then(|mut st| st.execute(params![
                a.id, project_id,
                a.path.to_string_lossy().to_string(),
                a.companion_path.as_ref().map(|p| p.to_string_lossy().to_string()),
                kind, a.size_bytes as i64, a.mtime,
                a.content_hash, a.phash.map(|h| h as i64),
                a.width.map(|w| w as i64), a.height.map(|h| h as i64),
                a.duration_secs, a.taken_at, a.tz_offset_secs, i64::from(a.time_guessed),
                a.gps.map(|g| g.0), a.gps.map(|g| g.1),
                a.rotation_deg as i64, conf, a.user_caption,
            ]),
        ).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn list_assets(&self, project_id: &str) -> Result<Vec<Asset>, String> {
        let mut st = self
            .conn
            .prepare(
                "SELECT id,path,companion_path,kind,size_bytes,mtime,content_hash,phash,width,height,duration_secs,taken_at,tz_offset,time_guessed,lat,lon,rotation_deg,pair_confidence,user_caption FROM assets WHERE project_id=?1 ORDER BY taken_at NULLS LAST, id",
            )
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map(params![project_id], |r| {
                let kind: String = r.get(3)?;
                let conf: Option<String> = r.get(17)?;
                let (lat, lon): (Option<f64>, Option<f64>) = (r.get(14)?, r.get(15)?);
                Ok(Asset {
                    id: r.get(0)?,
                    path: PathBuf::from(r.get::<_, String>(1)?),
                    companion_path: r.get::<_, Option<String>>(2)?.map(PathBuf::from),
                    kind: match kind.as_str() {
                        "video" => AssetKind::Video,
                        "livephoto" => AssetKind::LivePhoto,
                        _ => AssetKind::Photo,
                    },
                    size_bytes: r.get::<_, i64>(4)? as u64,
                    mtime: r.get(5)?,
                    content_hash: r.get(6)?,
                    phash: r.get::<_, Option<i64>>(7)?.map(|h| h as u64),
                    width: r.get::<_, Option<i64>>(8)?.map(|w| w as u32),
                    height: r.get::<_, Option<i64>>(9)?.map(|h| h as u32),
                    duration_secs: r.get(10)?,
                    taken_at: r.get(11)?,
                    tz_offset_secs: r.get(12)?,
                    time_guessed: r.get::<_, i64>(13)? != 0,
                    gps: match (lat, lon) {
                        (Some(a), Some(o)) => Some((a, o)),
                        _ => None,
                    },
                    rotation_deg: r.get::<_, i64>(16)? as u16,
                    pairing_confidence: match conf.as_deref() {
                        Some("content-id") => Some(PairingConfidence::ContentId),
                        Some("basename-only") => Some(PairingConfidence::BasenameOnly),
                        _ => None,
                    },
                    user_caption: r.get(18)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    // -- clusters ---------------------------------------------------------
    pub fn insert_cluster(&self, project_id: &str, kind: &str, c: &Cluster) -> Result<(), String> {
        let gps = c.centroid_gps;
        self.conn
            .prepare_cached(
                "INSERT OR REPLACE INTO clusters (id,project_id,kind,label,start_time,end_time,lat,lon) VALUES (?,?,?,?,?,?,?,?)",
            )
            .and_then(|mut st| {
                st.execute(params![
                    c.id, project_id, kind, c.label, c.start_time, c.end_time,
                    gps.map(|g| g.0), gps.map(|g| g.1)
                ])
            })
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn link_asset_cluster(
        &self,
        asset_id: &str,
        cluster_id: &str,
        is_hero: bool,
    ) -> Result<(), String> {
        self.conn
            .prepare_cached(
                "INSERT OR REPLACE INTO asset_clusters (asset_id,cluster_id,is_hero) VALUES (?,?,?)",
            )
            .and_then(|mut st| st.execute(params![asset_id, cluster_id, i64::from(is_hero)]))
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn hero_ids(&self, cluster_id: &str) -> Result<Vec<String>, String> {
        let mut st = self
            .conn
            .prepare("SELECT asset_id FROM asset_clusters WHERE cluster_id=?1 AND is_hero=1")
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map(params![cluster_id], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<String>, _>>()
            .map_err(|e| e.to_string())
    }

    // -- plans ------------------------------------------------------------
    /// Stored Story-editor edits for a project (empty when never edited).
    pub fn get_plan_edits(&self, project_id: &str) -> Result<crate::plan::PlanEdits, String> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT edits_json FROM plan_edits WHERE project_id=?1",
                params![project_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        match json {
            Some(j) => serde_json::from_str(&j).map_err(|e| e.to_string()),
            None => Ok(crate::plan::PlanEdits::default()),
        }
    }

    /// Persist Story-editor edits for a project.
    pub fn save_plan_edits(
        &self,
        project_id: &str,
        edits: &crate::plan::PlanEdits,
    ) -> Result<(), String> {
        let json = serde_json::to_string(edits).map_err(|e| e.to_string())?;
        self.conn
            .execute(
                "INSERT INTO plan_edits (project_id, edits_json) VALUES (?1, ?2)
                 ON CONFLICT(project_id) DO UPDATE SET edits_json=excluded.edits_json",
                params![project_id, json],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn insert_plan(
        &self,
        project_id: &str,
        plan: &Plan,
        source: &str,
    ) -> Result<String, String> {
        let id = new_id("plan");
        let json = plan.to_json().map_err(|e| e.to_string())?;
        self.conn
            .execute(
                "INSERT INTO plans (id,project_id,plan_json,plan_version,created_at,source) VALUES (?,?,?,?,?,?)",
                params![id, project_id, json, plan.plan_version as i64, now(), source],
            )
            .map_err(|e| e.to_string())?;
        Ok(id)
    }

    /// Latest plan for `project_id`, or across all projects when `None`
    /// (CLI convenience for single-project DBs).
    pub fn latest_plan(&self, project_id: Option<&str>) -> Result<Option<Plan>, String> {
        Ok(self.latest_plan_with_project(project_id)?.map(|(_, p)| p))
    }

    /// Latest plan plus its project id (export convenience for
    /// single-project DBs). Returns `None` when no plan exists.
    pub fn latest_plan_with_project(
        &self,
        project_id: Option<&str>,
    ) -> Result<Option<(String, Plan)>, String> {
        let row: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT project_id, plan_json FROM plans WHERE ?1 IS NULL OR project_id=?1 ORDER BY created_at DESC, rowid DESC LIMIT 1",
                params![project_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        row.map(|(pid, j)| {
            Plan::from_json(&j)
                .map(|p| (pid, p))
                .map_err(|e| e.to_string())
        })
        .transpose()
    }

    /// Latest plan with the project's Story-editor edits applied: `edited`
    /// is what every keepsake renders. The stored plan stays pristine, so
    /// edits can be undone and carry over to newer plans of the project.
    pub fn edited_plan(&self, project_id: Option<&str>) -> Result<Option<EditedPlan>, String> {
        let Some((project_id, base)) = self.latest_plan_with_project(project_id)? else {
            return Ok(None);
        };
        let edits = self.get_plan_edits(&project_id)?;
        let assets = self.list_assets(&project_id)?;
        let edited = crate::plan::apply_edits(&base, &assets, &edits);
        Ok(Some(EditedPlan {
            project_id,
            base,
            edited,
            edits,
            assets,
        }))
    }

    // -- jobs -------------------------------------------------------------
    pub fn insert_job(
        &self,
        project_id: &str,
        kind: &str,
        temp_dir: Option<&str>,
    ) -> Result<Job, String> {
        let t = now();
        let job = Job {
            id: new_id("job"),
            kind: kind.to_string(),
            state: JobState::Queued,
            progress: 0.0,
            current_step: None,
            temp_dir: temp_dir.map(|s| s.to_string()),
            error: None,
        };
        self.conn
            .execute(
                "INSERT INTO jobs (id,project_id,kind,state,progress,created_at,updated_at,temp_dir) VALUES (?,?,?,?,?,?,?,?)",
                params![job.id, project_id, job.kind, job_state_to_str(job.state), job.progress, t, t, job.temp_dir],
            )
            .map_err(|e| e.to_string())?;
        Ok(job)
    }

    pub fn get_job(&self, id: &str) -> Result<Option<Job>, String> {
        self.conn
            .query_row(
                &format!("SELECT {JOB_COLS} FROM jobs WHERE id=?1"),
                params![id],
                job_from_row,
            )
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn update_job(
        &self,
        id: &str,
        state: JobState,
        progress: f32,
        step: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), String> {
        let s = job_state_to_str(state);
        self.conn
            .execute(
                "UPDATE jobs SET state=?1, progress=?2, current_step=?3, error=?4, updated_at=?5 WHERE id=?6",
                params![s, progress, step, error, now(), id],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn jobs_in_state(&self, states: &[JobState]) -> Result<Vec<Job>, String> {
        let names: Vec<&str> = states.iter().map(|s| job_state_to_str(*s)).collect();
        let placeholders = names.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!("SELECT {JOB_COLS} FROM jobs WHERE state IN ({placeholders})");
        let mut st = self.conn.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = st
            .query_map(rusqlite::params_from_iter(names), job_from_row)
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }
}

pub fn job_state_to_str(s: JobState) -> &'static str {
    match s {
        JobState::Queued => "queued",
        JobState::Running => "running",
        JobState::Finalizing => "finalizing",
        JobState::Done => "done",
        JobState::Failed => "failed",
        JobState::Cancelled => "cancelled",
        JobState::Interrupted => "interrupted",
    }
}

pub fn job_state_from_str(s: &str) -> JobState {
    match s {
        "running" => JobState::Running,
        "finalizing" => JobState::Finalizing,
        "done" => JobState::Done,
        "failed" => JobState::Failed,
        "cancelled" => JobState::Cancelled,
        "interrupted" => JobState::Interrupted,
        _ => JobState::Queued,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Db {
        Db::open_in_memory().unwrap()
    }

    #[test]
    fn plan_edits_round_trip_and_survive_rescan() {
        let db = Db::open_in_memory().unwrap();
        let p = db.create_project("trip", "/tmp/trip").unwrap();
        let edits = crate::plan::PlanEdits {
            excluded: vec!["a1".into()],
            swaps: vec![("a2".into(), "a3".into())],
            place_overrides: [("a1".to_string(), "Our Hotel".to_string())].into(),
            caption_overrides: [("a3".to_string(), "Sunset".to_string())].into(),
        };
        assert!(db.get_plan_edits(&p.id).unwrap().is_empty());
        db.save_plan_edits(&p.id, &edits).unwrap();
        assert_eq!(db.get_plan_edits(&p.id).unwrap(), edits);

        let assets: Vec<Asset> = ["a1", "a2"]
            .iter()
            .map(|id| {
                let mut a = Asset::new(
                    *id,
                    PathBuf::from(format!("/t/{id}.jpg")),
                    crate::types::AssetKind::Photo,
                );
                a.taken_at = Some(1_714_557_600);
                a
            })
            .collect();
        for a in &assets {
            db.upsert_asset(&p.id, a).unwrap();
        }
        let cluster_of = assets
            .iter()
            .map(|a| (a.id.clone(), "2024-05-01".to_string()))
            .collect();
        let plan = crate::plan::build_plan(&assets, &cluster_of, &Default::default());
        let members = |p: &Plan| p.chapters.iter().map(|c| c.asset_ids.len()).sum::<usize>();
        assert_eq!(members(&plan), 2);
        db.insert_plan(&p.id, &plan, "local").unwrap();

        let excluded = crate::plan::PlanEdits {
            excluded: vec!["a1".into()],
            ..Default::default()
        };
        db.save_plan_edits(&p.id, &excluded).unwrap();
        // A newer plan is a fresh base; the edit still applies on read.
        db.insert_plan(&p.id, &plan, "local").unwrap();
        let edited = db.edited_plan(Some(&p.id)).unwrap().unwrap();
        assert_eq!(edited.project_id, p.id);
        assert_eq!(members(&edited.edited), 1);
        // The stored plan stays pristine, so the removal can be undone.
        assert_eq!(members(&edited.base), 2);
        db.save_plan_edits(&p.id, &Default::default()).unwrap();
        let restored = db.edited_plan(Some(&p.id)).unwrap().unwrap();
        assert_eq!(members(&restored.edited), 2);
    }

    #[test]
    fn init_adds_tz_offset_to_older_databases_once() {
        let conn = Connection::open_in_memory().unwrap();
        let old_schema = SCHEMA.replace("    tz_offset INTEGER,\n", "");
        assert_ne!(old_schema, SCHEMA);
        conn.execute_batch(&old_schema).unwrap();
        let db = Db::init(conn).unwrap();
        // Re-running init on the migrated store must not fail.
        let db = Db::init(db.conn).unwrap();
        let columns: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('assets') WHERE name='tz_offset'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(columns, 1);
    }

    fn asset(id: &str) -> Asset {
        let mut a = Asset::new(
            id,
            PathBuf::from(format!("/tmp/{id}.jpg")),
            AssetKind::LivePhoto,
        );
        a.size_bytes = 42;
        a.mtime = 1_714_557_600;
        a.content_hash = Some("abc".into());
        a.phash = Some(7);
        a.companion_path = Some(format!("/tmp/{id}.mov").into());
        a.pairing_confidence = Some(PairingConfidence::BasenameOnly);
        a.gps = Some((37.7, -122.4));
        a.taken_at = Some(1_714_557_600);
        a
    }

    #[test]
    fn project_asset_plan_round_trip() {
        let db = mem();
        let p = db.create_project("trip", "/tmp/trip").unwrap();
        assert!(db.get_project(&p.id).unwrap().is_some());
        db.upsert_asset(&p.id, &asset("a1")).unwrap();
        db.upsert_asset(&p.id, &asset("a1")).unwrap(); // idempotent
        let assets = db.list_assets(&p.id).unwrap();
        assert_eq!(assets.len(), 1);
        let a = &assets[0];
        assert_eq!(a.kind, AssetKind::LivePhoto);
        assert_eq!(a.pairing_confidence, Some(PairingConfidence::BasenameOnly));
        assert_eq!(
            a.companion_path.as_ref().unwrap().to_string_lossy(),
            "/tmp/a1.mov"
        );

        let c = Cluster {
            id: "c001".into(),
            asset_ids: vec!["a1".into()],
            start_time: Some(1),
            end_time: Some(2),
            centroid_gps: a.gps,
            label: "2024-05-01".into(),
        };
        db.insert_cluster(&p.id, "day", &c).unwrap();
        db.link_asset_cluster("a1", "c001", true).unwrap();
        assert_eq!(db.hero_ids("c001").unwrap(), vec!["a1".to_string()]);

        let plan = Plan {
            plan_version: 1,
            schema_version: Plan::SCHEMA_VERSION,
            hero: vec![],
            reel_selects: vec![],
            chapters: vec![],
            captions: Default::default(),
            map_polyline: vec![],
            music: crate::types::MusicPick {
                track: "t".into(),
                rationale: "r".into(),
            },
        };
        db.insert_plan(&p.id, &plan, "local").unwrap();
        let back = db.latest_plan(Some(&p.id)).unwrap().unwrap();
        assert_eq!(back.plan_version, 1);
        assert_eq!(back.schema_version, Plan::SCHEMA_VERSION);
        assert!(
            db.latest_plan(None).unwrap().is_some(),
            "any-project lookup"
        );
        assert!(db.latest_plan(Some("prj_missing")).unwrap().is_none());
        let (pid, _) = db.latest_plan_with_project(None).unwrap().unwrap();
        assert_eq!(pid, p.id, "any-project lookup reports the owning project");

        // Newest insert wins even with a lower plan_version and the same
        // created_at second.
        let older_v = Plan {
            plan_version: 0,
            music: crate::types::MusicPick {
                track: "newest".into(),
                rationale: "r".into(),
            },
            ..plan
        };
        db.insert_plan(&p.id, &older_v, "local").unwrap();
        assert_eq!(
            db.latest_plan(Some(&p.id)).unwrap().unwrap().music.track,
            "newest"
        );
    }

    #[test]
    fn job_crud_round_trip() {
        let db = mem();
        let p = db.create_project("t", "/x").unwrap();
        let j = db.insert_job(&p.id, "render-video", None).unwrap();
        assert_eq!(j.state, JobState::Queued);
        db.update_job(&j.id, JobState::Running, 0.5, Some("xfade"), None)
            .unwrap();
        let back = db.get_job(&j.id).unwrap().unwrap();
        assert_eq!(back.state, JobState::Running);
        assert_eq!(back.current_step.as_deref(), Some("xfade"));
        assert!(db.get_job("job_missing").unwrap().is_none());
    }

    #[test]
    fn transaction_commits_or_rolls_back() {
        let db = mem();
        let p = db.create_project("t", "/x").unwrap();
        let err: Result<(), String> = db.in_transaction(|db| {
            db.upsert_asset(&p.id, &asset("rolled"))?;
            Err("boom".into())
        });
        assert_eq!(err.unwrap_err(), "boom");
        assert!(db.list_assets(&p.id).unwrap().is_empty(), "rolled back");
        db.in_transaction(|db| db.upsert_asset(&p.id, &asset("kept")))
            .unwrap();
        assert_eq!(db.list_assets(&p.id).unwrap().len(), 1);
    }
}
