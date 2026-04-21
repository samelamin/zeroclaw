// Task planner: dependency DAG for multi-agent task decomposition.
//
// Provides `TaskPlan` (a directed acyclic graph of sub-tasks) with
// topological ordering, parallel batch grouping, and an execution
// state tracker (`PlanExecution`) that enriches downstream prompts
// with upstream results.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

/// A single unit of work assigned to an agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubTask {
    pub id: String,
    pub description: String,
    pub agent: String,
    pub prompt: String,
    pub depends_on: Vec<String>,
    pub priority: i32,
}

/// A plan consisting of sub-tasks linked by dependency edges.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskPlan {
    pub goal: String,
    pub tasks: Vec<SubTask>,
}

// ---------------------------------------------------------------------------
// TaskPlan implementation
// ---------------------------------------------------------------------------

impl TaskPlan {
    /// Validate the plan structure.
    ///
    /// Checks for:
    /// - empty task list
    /// - self-dependencies
    /// - references to non-existent tasks
    /// - dependency cycles (via topological sort)
    pub fn validate(&self) -> Result<(), String> {
        if self.tasks.is_empty() {
            return Err("task plan has no tasks".to_string());
        }

        let ids: HashSet<&str> = self.tasks.iter().map(|t| t.id.as_str()).collect();

        for task in &self.tasks {
            for dep in &task.depends_on {
                if dep == &task.id {
                    return Err(format!("task '{}' depends on itself", task.id));
                }
                if !ids.contains(dep.as_str()) {
                    return Err(format!(
                        "task '{}' depends on non-existent task '{}'",
                        task.id, dep
                    ));
                }
            }
        }

        if self.topological_order().is_none() {
            return Err("task plan contains a cycle".to_string());
        }

        Ok(())
    }

    /// Return tasks in a valid topological order using Kahn's algorithm.
    ///
    /// Returns `None` when the graph contains a cycle.
    pub fn topological_order(&self) -> Option<Vec<&SubTask>> {
        let index: HashMap<&str, usize> = self
            .tasks
            .iter()
            .enumerate()
            .map(|(i, t)| (t.id.as_str(), i))
            .collect();

        let n = self.tasks.len();
        let mut in_degree = vec![0u32; n];
        let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); n];

        for (i, task) in self.tasks.iter().enumerate() {
            for dep in &task.depends_on {
                if let Some(&dep_idx) = index.get(dep.as_str()) {
                    adjacency[dep_idx].push(i);
                    in_degree[i] += 1;
                }
            }
        }

        let mut queue: VecDeque<usize> = VecDeque::new();
        for (i, &deg) in in_degree.iter().enumerate() {
            if deg == 0 {
                queue.push_back(i);
            }
        }

        let mut order: Vec<&SubTask> = Vec::with_capacity(n);
        while let Some(idx) = queue.pop_front() {
            order.push(&self.tasks[idx]);
            for &next in &adjacency[idx] {
                in_degree[next] -= 1;
                if in_degree[next] == 0 {
                    queue.push_back(next);
                }
            }
        }

        if order.len() == n { Some(order) } else { None }
    }

    /// Return tasks whose dependencies are all satisfied and that are not
    /// themselves in the `completed` set.
    pub fn ready_tasks<'a>(&'a self, completed: &[String]) -> Vec<&'a SubTask> {
        let done: HashSet<&str> = completed.iter().map(|s| s.as_str()).collect();
        self.tasks
            .iter()
            .filter(|t| {
                !done.contains(t.id.as_str())
                    && t.depends_on.iter().all(|d| done.contains(d.as_str()))
            })
            .collect()
    }

    /// Group tasks into sequential batches that can each be executed in
    /// parallel.
    ///
    /// Returns `None` if the graph contains a cycle.
    pub fn execution_batches(&self) -> Option<Vec<Vec<&SubTask>>> {
        let index: HashMap<&str, usize> = self
            .tasks
            .iter()
            .enumerate()
            .map(|(i, t)| (t.id.as_str(), i))
            .collect();

        let n = self.tasks.len();
        let mut in_degree = vec![0u32; n];
        let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); n];

        for (i, task) in self.tasks.iter().enumerate() {
            for dep in &task.depends_on {
                if let Some(&dep_idx) = index.get(dep.as_str()) {
                    adjacency[dep_idx].push(i);
                    in_degree[i] += 1;
                }
            }
        }

        let mut queue: VecDeque<usize> = VecDeque::new();
        for (i, &deg) in in_degree.iter().enumerate() {
            if deg == 0 {
                queue.push_back(i);
            }
        }

        let mut batches: Vec<Vec<&SubTask>> = Vec::new();
        let mut visited = 0usize;

        while !queue.is_empty() {
            let current_batch_indices: Vec<usize> = queue.drain(..).collect();
            let mut batch: Vec<&SubTask> = Vec::new();

            for &idx in &current_batch_indices {
                visited += 1;
                batch.push(&self.tasks[idx]);
                for &next in &adjacency[idx] {
                    in_degree[next] -= 1;
                    if in_degree[next] == 0 {
                        queue.push_back(next);
                    }
                }
            }

            batch.sort_by(|a, b| a.priority.cmp(&b.priority).then_with(|| a.id.cmp(&b.id)));
            batches.push(batch);
        }

        if visited == n { Some(batches) } else { None }
    }
}

// ---------------------------------------------------------------------------
// PlanExecution state tracker
// ---------------------------------------------------------------------------

/// Tracks the runtime state of a [`TaskPlan`] as tasks complete or fail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanExecution {
    pub plan: TaskPlan,
    pub completed: Vec<String>,
    pub failed: Vec<(String, String)>,
    pub results: HashMap<String, String>,
}

impl PlanExecution {
    /// Create a new execution tracker for the given plan.
    pub fn new(plan: TaskPlan) -> Self {
        Self {
            plan,
            completed: Vec::new(),
            failed: Vec::new(),
            results: HashMap::new(),
        }
    }

    /// Mark a task as completed, storing its result output.
    pub fn complete(&mut self, task_id: &str, result: String) {
        self.results.insert(task_id.to_string(), result);
        self.completed.push(task_id.to_string());
    }

    /// Mark a task as failed with an error message.
    pub fn fail(&mut self, task_id: &str, error: String) {
        self.failed.push((task_id.to_string(), error));
    }

    /// Returns `true` when every task is either completed or failed.
    pub fn is_finished(&self) -> bool {
        let started: HashSet<&str> = self
            .completed
            .iter()
            .chain(self.failed.iter().map(|(id, _)| id))
            .map(|s| s.as_str())
            .collect();
        self.plan
            .tasks
            .iter()
            .all(|t| started.contains(t.id.as_str()))
    }

    /// Returns `true` when every task completed successfully.
    pub fn is_success(&self) -> bool {
        self.failed.is_empty() && self.completed.len() == self.plan.tasks.len()
    }

    /// Return tasks that are ready to run: all dependencies satisfied (and not
    /// failed), task not already completed or failed.
    pub fn ready_tasks(&self) -> Vec<&SubTask> {
        let done: HashSet<&str> = self.completed.iter().map(|s| s.as_str()).collect();
        let failed_ids: HashSet<&str> = self.failed.iter().map(|(id, _)| id.as_str()).collect();
        let started: HashSet<&str> = done
            .iter()
            .copied()
            .chain(failed_ids.iter().copied())
            .collect();

        self.plan
            .tasks
            .iter()
            .filter(|t| {
                !started.contains(t.id.as_str())
                    && t.depends_on
                        .iter()
                        .all(|d| done.contains(d.as_str()) && !failed_ids.contains(d.as_str()))
            })
            .collect()
    }

    /// Build an enriched prompt for a task by injecting the results of its
    /// completed dependencies.
    pub fn enriched_prompt(&self, task: &SubTask) -> String {
        let mut sections: Vec<String> = Vec::new();
        for dep_id in &task.depends_on {
            if let Some(result) = self.results.get(dep_id) {
                sections.push(format!("[Result from '{}']\n{}", dep_id, result));
            }
        }
        if sections.is_empty() {
            task.prompt.clone()
        } else {
            format!("{}\n\n{}", sections.join("\n\n"), task.prompt)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(id: &str, deps: &[&str], agent: &str, priority: i32) -> SubTask {
        SubTask {
            id: id.to_string(),
            description: format!("{} task", id),
            agent: agent.to_string(),
            prompt: format!("Do {}", id),
            depends_on: deps.iter().map(|d| d.to_string()).collect(),
            priority,
        }
    }

    // ---- TaskPlan tests ----

    #[test]
    fn valid_linear_plan() {
        let plan = TaskPlan {
            goal: "linear".into(),
            tasks: vec![
                sub("a", &[], "agent1", 0),
                sub("b", &["a"], "agent1", 0),
                sub("c", &["b"], "agent1", 0),
            ],
        };
        assert!(plan.validate().is_ok());
    }

    #[test]
    fn valid_parallel_plan() {
        let plan = TaskPlan {
            goal: "parallel".into(),
            tasks: vec![
                sub("a", &[], "agent1", 0),
                sub("b", &[], "agent2", 0),
                sub("c", &["a", "b"], "agent3", 0),
            ],
        };
        assert!(plan.validate().is_ok());
        let batches = plan.execution_batches().unwrap();
        assert_eq!(batches.len(), 2);
        let first_ids: HashSet<&str> = batches[0].iter().map(|t| t.id.as_str()).collect();
        assert!(first_ids.contains("a"));
        assert!(first_ids.contains("b"));
        assert_eq!(batches[1].len(), 1);
        assert_eq!(batches[1][0].id, "c");
    }

    #[test]
    fn rejects_cycle() {
        let plan = TaskPlan {
            goal: "cyclic".into(),
            tasks: vec![sub("a", &["b"], "x", 0), sub("b", &["a"], "x", 0)],
        };
        let err = plan.validate().unwrap_err();
        assert!(err.contains("cycle"), "expected cycle error, got: {err}");
    }

    #[test]
    fn rejects_missing_dep() {
        let plan = TaskPlan {
            goal: "missing".into(),
            tasks: vec![sub("a", &["nonexistent"], "x", 0)],
        };
        let err = plan.validate().unwrap_err();
        assert!(err.contains("non-existent"), "got: {err}");
    }

    #[test]
    fn rejects_self_dep() {
        let plan = TaskPlan {
            goal: "self".into(),
            tasks: vec![sub("a", &["a"], "x", 0)],
        };
        let err = plan.validate().unwrap_err();
        assert!(err.contains("itself"), "got: {err}");
    }

    #[test]
    fn rejects_empty_plan() {
        let plan = TaskPlan {
            goal: "empty".into(),
            tasks: vec![],
        };
        let err = plan.validate().unwrap_err();
        assert!(err.contains("no tasks"), "got: {err}");
    }

    #[test]
    fn ready_tasks_respects_completed() {
        let plan = TaskPlan {
            goal: "ready".into(),
            tasks: vec![
                sub("a", &[], "x", 0),
                sub("b", &["a"], "x", 0),
                sub("c", &["b"], "x", 0),
            ],
        };

        // Initially only "a" is ready.
        let ready = plan.ready_tasks(&[]);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "a");

        // After "a" completes, "b" is ready.
        let ready = plan.ready_tasks(&["a".into()]);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "b");

        // After "a" and "b" complete, "c" is ready.
        let ready = plan.ready_tasks(&["a".into(), "b".into()]);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "c");
    }

    #[test]
    fn topological_order_correct() {
        let plan = TaskPlan {
            goal: "topo".into(),
            tasks: vec![
                sub("c", &["b"], "x", 0),
                sub("a", &[], "x", 0),
                sub("b", &["a"], "x", 0),
            ],
        };
        let order = plan.topological_order().unwrap();
        let ids: Vec<&str> = order.iter().map(|t| t.id.as_str()).collect();

        // "a" must precede "b", "b" must precede "c"
        let pos_a = ids.iter().position(|&id| id == "a").unwrap();
        let pos_b = ids.iter().position(|&id| id == "b").unwrap();
        let pos_c = ids.iter().position(|&id| id == "c").unwrap();
        assert!(pos_a < pos_b);
        assert!(pos_b < pos_c);
    }

    #[test]
    fn execution_batches_naseyma_workflow() {
        // Simulates 7-agent Naseyma workflow:
        // brief -> research -> [pitch, strategy] -> design -> content -> qa
        let plan = TaskPlan {
            goal: "naseyma website generation".into(),
            tasks: vec![
                sub("brief", &[], "brief_agent", 0),
                sub("research", &["brief"], "research_agent", 0),
                sub("pitch", &["research"], "pitch_agent", 0),
                sub("strategy", &["research"], "strategy_agent", 0),
                sub("design", &["pitch", "strategy"], "design_agent", 0),
                sub("content", &["design"], "content_agent", 0),
                sub("qa", &["content"], "qa_agent", 0),
            ],
        };

        assert!(plan.validate().is_ok());
        let batches = plan.execution_batches().unwrap();

        // Expected batches:
        // 1: [brief]
        // 2: [research]
        // 3: [pitch, strategy]  (parallel)
        // 4: [design]
        // 5: [content]
        // 6: [qa]
        assert_eq!(batches.len(), 6);

        let batch_ids: Vec<Vec<&str>> = batches
            .iter()
            .map(|b| b.iter().map(|t| t.id.as_str()).collect())
            .collect();

        assert_eq!(batch_ids[0], vec!["brief"]);
        assert_eq!(batch_ids[1], vec!["research"]);
        let mut b2 = batch_ids[2].clone();
        b2.sort();
        assert_eq!(b2, vec!["pitch", "strategy"]);
        assert_eq!(batch_ids[3], vec!["design"]);
        assert_eq!(batch_ids[4], vec!["content"]);
        assert_eq!(batch_ids[5], vec!["qa"]);
    }

    // ---- PlanExecution tests ----

    #[test]
    fn plan_execution_lifecycle() {
        let plan = TaskPlan {
            goal: "lifecycle".into(),
            tasks: vec![
                sub("a", &[], "x", 0),
                sub("b", &["a"], "x", 0),
                sub("c", &["b"], "x", 0),
            ],
        };
        let mut exec = PlanExecution::new(plan);

        assert!(!exec.is_finished());
        assert!(!exec.is_success());

        exec.complete("a", "result_a".into());
        assert!(!exec.is_finished());

        exec.complete("b", "result_b".into());
        assert!(!exec.is_finished());

        exec.complete("c", "result_c".into());
        assert!(exec.is_finished());
        assert!(exec.is_success());
    }

    #[test]
    fn enriched_prompt_includes_dependency_results() {
        let plan = TaskPlan {
            goal: "enrich".into(),
            tasks: vec![
                sub("a", &[], "x", 0),
                sub("b", &[], "x", 0),
                sub("c", &["a", "b"], "x", 0),
            ],
        };
        let mut exec = PlanExecution::new(plan);
        exec.complete("a", "output_a".into());
        exec.complete("b", "output_b".into());

        let task_c = &exec.plan.tasks[2];
        let prompt = exec.enriched_prompt(task_c);

        assert!(prompt.contains("output_a"), "missing dep a result");
        assert!(prompt.contains("output_b"), "missing dep b result");
        assert!(prompt.contains("Do c"), "missing original prompt");
    }

    #[test]
    fn failed_task_blocks_dependents() {
        let plan = TaskPlan {
            goal: "failure".into(),
            tasks: vec![
                sub("a", &[], "x", 0),
                sub("b", &["a"], "x", 0),
                sub("c", &["b"], "x", 0),
            ],
        };
        let mut exec = PlanExecution::new(plan);

        // "a" is ready initially.
        let ready = exec.ready_tasks();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "a");

        // Fail "a" — "b" should NOT become ready.
        exec.fail("a", "boom".into());
        let ready = exec.ready_tasks();
        assert!(ready.is_empty(), "b should be blocked by failed a");

        // The plan is not finished because "b" and "c" never ran.
        assert!(!exec.is_finished());
        assert!(!exec.is_success());
    }
}
