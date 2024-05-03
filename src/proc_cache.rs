use procfs::process::all_processes;
use std::collections::HashMap;

pub struct ProcCache {
    parent_map: HashMap<i32, i32>,
    result_cache: HashMap<i32, bool>,
    my_pid: i32,
}

impl ProcCache {
    pub fn new() -> Self {
        Self {
            parent_map: HashMap::new(),
            result_cache: HashMap::new(),
            my_pid: std::process::id() as i32,
        }
    }

    fn update_cache(&mut self) -> Result<(), procfs::ProcError> {
        let all_procs = all_processes()?;
        self.parent_map.clear();
        self.result_cache.clear();

        for proc in all_procs {
            if let Ok(stat) = proc.and_then(|x| x.stat()) {
                self.parent_map.insert(stat.pid, stat.ppid);
            }
        }
        Ok(())
    }

    pub fn is_child_or_grandchild(&mut self, target_pid: i32) -> bool {
        if let Some(&result) = self.result_cache.get(&target_pid) {
            return result;
        } else {
            if let Err(err) = self.update_cache() {
                log::error!("Could not update process cache: {err}");
            }
        }

        let mut current_pid = target_pid;

        while let Some(&parent_pid) = self.parent_map.get(&current_pid) {
            if parent_pid == self.my_pid {
                self.result_cache.insert(target_pid, true);
                return true;
            }
            if parent_pid == 1 || parent_pid == 0 {
                break;
            }
            current_pid = parent_pid;
        }

        self.result_cache.insert(target_pid, false);
        false
    }
}
