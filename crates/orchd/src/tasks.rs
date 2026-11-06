//! Stable workspace targets and conservative, serialized dispatch.
use crate::detect::{self, Detected};
use crate::pane::Pane;
use crate::server::{err, ApiError, Daemon};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub fn parts(target: &str) -> (&str, Option<&str>) {
    target
        .rsplit_once('@')
        .map_or((target, None), |(w, t)| (w, Some(t)))
}
impl Daemon {
    pub(crate) fn task_target(&self, target: &str) -> Result<Vec<Arc<Pane>>, ApiError> {
        let (label, tab) = parts(target);
        let session = self.session();
        let w = session
            .workspaces
            .iter()
            .find(|w| w.label == label)
            .ok_or_else(|| err("no_target", "unknown workspace label"))?;
        if tab.is_some_and(|id| !w.tabs.iter().any(|t| t.id == id)) {
            return Err(err("no_target", "unknown tab id"));
        }
        Ok(w.tabs
            .iter()
            .filter(|t| tab.is_none_or(|id| id == t.id))
            .flat_map(|t| t.root.panes())
            .filter_map(|id| self.find(&id).ok())
            .filter(|p| p.exit_code().is_none() && p.agent(&p.visible_text()).is_some())
            .collect())
    }
    fn task_ready(&self, p: &Pane) -> bool {
        if self.bus.task_input_uncertain(&p.id).unwrap_or(true)
            || p.exit_code().is_some()
            || p.detect() != Detected::Idle
        {
            return false;
        }
        let text = p.visible_text();
        if detect::model_switch_question(&text, p.agent(&text).as_deref()) {
            return false;
        }
        let Ok(agents) = self.bus.agents() else {
            return false;
        };
        let Ok(questions) = self.bus.questions(false) else {
            return false;
        };
        let pane = format!("orch:{}", p.id);
        !agents
            .iter()
            .filter(|a| a.pane.as_deref() == Some(&pane))
            .any(|a| {
                matches!(a.status.as_str(), "working" | "blocked")
                    || questions.iter().any(|q| q.from == a.name)
            })
    }
}

pub fn start(daemon: Arc<Daemon>) {
    tokio::spawn(async move {
        let idle = std::env::var("ORCHD_TASK_IDLE_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .map(Duration::from_millis)
            .unwrap_or(Duration::from_secs(3));
        let mut state = HashMap::new();
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        loop {
            tick.tick().await;
            if let Err(e) = check(&daemon, &mut state, idle).await {
                eprintln!("orchd tasks: {e:#}");
            }
        }
    });
}

async fn check(
    d: &Daemon,
    idle_since: &mut HashMap<String, Instant>,
    stable: Duration,
) -> anyhow::Result<()> {
    // Cancel/done cannot race the text/CR sequence. Also serializes task dispatch with nudges.
    let _gate = d.task_gate.lock().await;
    let panes = d.all_panes();
    idle_since.retain(|id, _| panes.iter().any(|p| &p.id == id));
    for p in &panes {
        if d.task_ready(p) {
            idle_since.entry(p.id.clone()).or_insert_with(Instant::now);
        } else {
            idle_since.remove(&p.id);
        }
    }
    let ready = |id: &str| idle_since.get(id).is_some_and(|at| at.elapsed() >= stable);
    for t in d
        .bus
        .tasks(false)?
        .into_iter()
        .filter(|t| t.state == "running")
    {
        let p = t
            .pane
            .as_deref()
            .and_then(|id| d.find(id).ok())
            .filter(|p| p.exit_code().is_none());
        let Some(p) = p else {
            d.bus.task_finish(t.id, "failed", Some("pane gone"))?;
            continue;
        };
        let screen = p.visible_text();
        let detected = detect::detect(&screen, p.agent(&screen).as_deref());
        if detected == Detected::Limited
            || detect::model_switch_question(&screen, p.agent(&screen).as_deref())
        {
            if t.saw_working {
                d.bus.task_paused(t.id)?;
            }
        } else if detected == Detected::Working {
            if !t.saw_working {
                d.bus.task_working(t.id)?;
            }
        } else if t.saw_working && ready(&p.id) && !d.bus.task_mcp(&p.id)? {
            d.bus.task_finish(t.id, "done", None)?;
        }
    }
    let mut dispatched = Vec::new();
    for t in d
        .bus
        .tasks(false)?
        .into_iter()
        .filter(|t| t.state == "queued")
    {
        let Ok(candidates) = d.task_target(&t.target) else {
            continue;
        };
        if candidates.len() != 1 {
            continue;
        }
        let p = &candidates[0];
        if dispatched.contains(&p.id) || !ready(&p.id) || !d.task_ready(p) {
            continue;
        }
        let explicit = d.bus.task_mcp(&p.id)?;
        let text = if explicit {
            format!(
                "[orch task #{}] {}  When finished, call the orch task_done tool with id {}.",
                t.id, t.text, t.id
            )
        } else {
            t.text.clone()
        };
        let paste = p.term.lock().unwrap().screen().bracketed_paste();
        if d.bus.task_claim(t.id, &p.id)?.is_none() {
            continue;
        }
        // Multiline input without bracketed paste could submit several prompts. Fail closed.
        if text.contains('\n') && !paste {
            d.bus.task_finish(
                t.id,
                "failed",
                Some("multiline input requires bracketed-paste support"),
            )?;
            continue;
        }
        let bytes = if paste {
            format!("\x1b[200~{text}\x1b[201~").into_bytes()
        } else {
            text.into_bytes()
        };
        let sent = {
            let term = p.term.lock().unwrap();
            let screen = term.visible_text();
            detect::detect(&screen, p.agent(&screen).as_deref()) == Detected::Idle && p.write(bytes)
        };
        tokio::time::sleep(Duration::from_millis(300)).await;
        let entered = sent && d.task_ready(p) && {
            let term = p.term.lock().unwrap();
            let screen = term.visible_text();
            detect::detect(&screen, p.agent(&screen).as_deref()) == Detected::Idle
                && p.write(b"\r".to_vec())
        };
        if !entered {
            d.bus.task_finish(
                t.id,
                "failed",
                Some("input interrupted; inspect pane before retrying"),
            )?;
        }
        dispatched.push(p.id.clone());
    }
    for id in dispatched {
        idle_since.remove(&id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::Bus;
    use orch_proto::api::NewPane;

    #[tokio::test]
    async fn isolated_pty_dispatches_only_after_observed_work_then_stable_idle() {
        let dir = std::env::temp_dir().join(format!(
            "orch-task-pty-{}-{}",
            std::process::id(),
            crate::bus::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let d = Daemon::new(100, Arc::new(Bus::in_memory().unwrap()));
        struct Guard(Arc<Daemon>, std::path::PathBuf);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.kill_all();
                let _ = std::fs::remove_dir_all(&self.1);
            }
        }
        let _guard = Guard(Arc::clone(&d), dir.clone());
        let script = r#"stty -echo
prompt() { printf '\033[2J\033[H› Ask Codex\n? for shortcuts\n'; }
prompt
while IFS= read -r line; do
 printf 'start:%s\n' "$line" >> received
 printf '\033[2J\033[HWorking (esc to interrupt)\n› Ask Codex\n'
 sleep 0.5
 printf 'end:%s\n' "$line" >> received
 prompt
done"#;
        let p = d
            .new_pane(NewPane {
                argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
                cwd: Some(dir.to_string_lossy().into()),
                env: Some(vec![("HOME".into(), dir.to_string_lossy().into())]),
                ..Default::default()
            })
            .unwrap();
        p.set_kind("codex");
        d.place_new_pane(&p.info()).unwrap();
        let target = d.session().workspaces[0].label.clone();
        let a = d.bus.task_add(&target, "first", "test", None).unwrap();
        let b = d
            .bus
            .task_add(&target, "second", "test", Some(a.id))
            .unwrap();
        let c = d.bus.task_add(&target, "cancelled", "test", None).unwrap();
        d.bus.task_finish(c.id, "cancelled", None).unwrap();
        let mut state = HashMap::new();
        for _ in 0..100 {
            check(&d, &mut state, Duration::from_millis(100))
                .await
                .unwrap();
            if d.bus.task(b.id).unwrap().unwrap().state == "done" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(d.bus.task(b.id).unwrap().unwrap().state, "done");
        assert_eq!(
            std::fs::read_to_string(dir.join("received")).unwrap(),
            "start:first\nend:first\nstart:second\nend:second\n"
        );
        let explicit = d.bus.task_add(&target, "explicit", "test", None).unwrap();
        d.bus.task_register_mcp(&p.id).unwrap();
        for _ in 0..35 {
            check(&d, &mut state, Duration::from_millis(100))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(d.bus.task(explicit.id).unwrap().unwrap().state, "running");
        assert!(d.bus.task(explicit.id).unwrap().unwrap().saw_working);
        d.bus.task_finish(explicit.id, "done", None).unwrap();
        for screen in [
            "You've hit your usage limit. Try again at 5:03 AM.\n› Ask Codex",
            "Switch to gpt-6-luna for lower credit usage?\n› 1. Switch to gpt-6-luna\n  2. Keep current model\nenter select · esc back",
            "Allow this command?\n› 1. Allow\nenter to submit | esc to cancel",
        ] {
            p.term.lock().unwrap().process(format!("\x1b[2J\x1b[H{screen}").as_bytes());
            assert!(!d.task_ready(&p), "must not dispatch to {screen}");
        }
        p.term
            .lock()
            .unwrap()
            .process("\x1b[2J\x1b[H› Ask Codex\n? for shortcuts".as_bytes());
        let agent = d
            .bus
            .register(&orch_proto::api::RegisterAgent {
                kind: "codex".into(),
                pane: Some(format!("orch:{}", p.id)),
                ..Default::default()
            })
            .unwrap();
        d.bus.ask(&agent.name, "Need Murat", &[]).unwrap();
        assert!(!d.task_ready(&p), "open human question blocks dispatch");
        let gone = d.bus.task_add(&target, "gone", "test", None).unwrap();
        d.bus.task_claim(gone.id, &p.id).unwrap();
        p.kill();
        for _ in 0..100 {
            check(&d, &mut state, Duration::ZERO).await.unwrap();
            if d.bus.task(gone.id).unwrap().unwrap().state == "failed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(d.bus.task(gone.id).unwrap().unwrap().state, "failed");
    }
    #[tokio::test]
    async fn multiline_is_one_bracketed_paste_with_a_separate_enter() {
        let dir = std::env::temp_dir().join(format!(
            "orch-task-paste-{}-{}",
            std::process::id(),
            crate::bus::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let d = Daemon::new(100, Arc::new(Bus::in_memory().unwrap()));
        struct Guard(Arc<Daemon>, std::path::PathBuf);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.kill_all();
                let _ = std::fs::remove_dir_all(&self.1);
            }
        }
        let _guard = Guard(Arc::clone(&d), dir.clone());
        let script = r#"import os, tty, time
tty.setraw(0)
def screen(text): os.write(1, ('\x1b[2J\x1b[H'+text).encode())
os.write(1,b'\x1b[?2004h')
screen('› Ask Codex\n? for shortcuts')
data=b''
while True:
    b=os.read(0,1)
    data+=b
    if data.endswith(b'\x1b[201~'): pasted=time.monotonic()
    if b==b'\r': break
open('bytes','wb').write(data)
open('gap','w').write(str(time.monotonic()-pasted))
screen('Working (esc to interrupt)\n› Ask Codex')
time.sleep(.5)
screen('› Ask Codex\n? for shortcuts')
while True: os.read(0,1024)"#
            .to_string();
        let p = d
            .new_pane(NewPane {
                argv: vec!["/usr/bin/python3".into(), "-c".into(), script],
                cwd: Some(dir.to_string_lossy().into()),
                env: Some(vec![("HOME".into(), dir.to_string_lossy().into())]),
                ..Default::default()
            })
            .unwrap();
        p.set_kind("codex");
        d.place_new_pane(&p.info()).unwrap();
        let text = "first line\nsecond line\n".repeat(500);
        let t = d
            .bus
            .task_add(&d.session().workspaces[0].label, &text, "test", None)
            .unwrap();
        let mut state = HashMap::new();
        for _ in 0..100 {
            check(&d, &mut state, Duration::from_millis(100))
                .await
                .unwrap();
            if d.bus.task(t.id).unwrap().unwrap().state == "done" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(d.bus.task(t.id).unwrap().unwrap().state, "done");
        assert_eq!(
            std::fs::read(dir.join("bytes")).unwrap(),
            format!("\x1b[200~{text}\x1b[201~\r").as_bytes()
        );
        let gap: f64 = std::fs::read_to_string(dir.join("gap"))
            .unwrap()
            .parse()
            .unwrap();
        assert!(gap >= 0.2, "Enter must arrive after paste: {gap}");
    }
}
