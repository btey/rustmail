//! Export and import of tasks as JSON (Phase 11).
//!
//! The file is portable: projects, columns, tasks, checklists, labels and
//! contexts are named, never numbered. Links to mail and events are local
//! references and are not exported. Importing adds to what exists: a task whose
//! title and description already exist in the same project is skipped, so
//! importing the same file twice changes nothing.

use super::*;
use serde::{Deserialize, Serialize};

pub const EXPORT_VERSION: u32 = 1;
/// Largest file the importer reads, and the most tasks it will create.
pub const MAX_IMPORT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_IMPORT_TASKS: usize = 50_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskExport {
    pub app: String,
    pub version: u32,
    pub exported_at: i64,
    pub projects: Vec<ExportProject>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportProject {
    pub name: String,
    pub colour: String,
    pub columns: Vec<ExportColumn>,
    pub tasks: Vec<ExportTask>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportColumn {
    pub name: String,
    pub kind: ColumnKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportTask {
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// Name of the column the task sits in.
    pub column: String,
    #[serde(default)]
    pub due_at: Option<i64>,
    #[serde(default)]
    pub start_at: Option<i64>,
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub assignee_email: Option<String>,
    #[serde(default)]
    pub assignee_name: Option<String>,
    #[serde(default)]
    pub labels: Vec<ExportLabel>,
    #[serde(default)]
    pub checklist: Vec<ExportChecklistItem>,
    #[serde(default)]
    pub completed_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportLabel {
    pub name: String,
    pub colour: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportChecklistItem {
    pub text: String,
    pub done: bool,
}

/// What an import did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportReport {
    pub projects_created: usize,
    pub tasks_created: usize,
    pub tasks_skipped: usize,
}

impl SqliteTaskStore<'_> {
    /// Every project with its columns, open and done tasks.
    pub fn export_all(&mut self, now_ms: i64) -> Result<TaskExport> {
        let contexts = self.list_contexts()?;
        let labels = self.list_labels()?;
        let mut projects = Vec::new();
        for project in self.list_projects()? {
            let columns = self.list_columns(project.id)?;
            let filter = TaskFilter {
                project_id: Some(project.id),
                include_done: true,
                ..Default::default()
            };
            let mut tasks = Vec::new();
            for task in self.list_tasks(&filter)? {
                let column = columns
                    .iter()
                    .find(|column| column.id == task.column_id)
                    .map(|column| column.name.clone())
                    .unwrap_or_default();
                tasks.push(ExportTask {
                    title: task.title.clone(),
                    description: task.description.clone(),
                    column,
                    due_at: task.due_at,
                    start_at: task.start_at,
                    priority: task.priority,
                    context: task.context_id.and_then(|id| {
                        contexts
                            .iter()
                            .find(|context| context.id == id)
                            .map(|context| context.name.clone())
                    }),
                    assignee_email: task.assignee_email.clone(),
                    assignee_name: task.assignee_name.clone(),
                    labels: task
                        .label_ids
                        .iter()
                        .filter_map(|id| labels.iter().find(|label| label.id == *id))
                        .map(|label| ExportLabel {
                            name: label.name.clone(),
                            colour: label.colour.clone(),
                        })
                        .collect(),
                    checklist: self
                        .list_checklist(task.id)?
                        .into_iter()
                        .map(|item| ExportChecklistItem {
                            text: item.text,
                            done: item.done,
                        })
                        .collect(),
                    completed_at: task.completed_at,
                });
            }
            projects.push(ExportProject {
                name: project.name,
                colour: project.colour,
                columns: columns
                    .into_iter()
                    .map(|column| ExportColumn {
                        name: column.name,
                        kind: column.kind,
                    })
                    .collect(),
                tasks,
            });
        }
        Ok(TaskExport {
            app: "rustmail".into(),
            version: EXPORT_VERSION,
            exported_at: now_ms,
            projects,
        })
    }

    /// Add the file's projects and tasks to the database.
    pub fn import_all(&mut self, export: &TaskExport) -> Result<ImportReport> {
        if export.version != EXPORT_VERSION {
            return Err(crate::CoreError::Other(format!(
                "unsupported task export version {}",
                export.version
            )));
        }
        let total: usize = export.projects.iter().map(|p| p.tasks.len()).sum();
        if total > MAX_IMPORT_TASKS {
            return Err(crate::CoreError::Other(format!(
                "the file holds {total} tasks; at most {MAX_IMPORT_TASKS} are imported at once"
            )));
        }
        let mut report = ImportReport::default();
        for project in &export.projects {
            let existing = self
                .list_projects()?
                .into_iter()
                .find(|candidate| candidate.name == project.name);
            let target = match existing {
                Some(found) => found,
                None => {
                    report.projects_created += 1;
                    self.create_project(&project.name, &project.colour)?
                }
            };
            let mut columns = self.list_columns(target.id)?;
            let present = self.list_tasks(&TaskFilter {
                project_id: Some(target.id),
                include_done: true,
                ..Default::default()
            })?;
            let mut known: std::collections::HashSet<(String, String)> = present
                .into_iter()
                .map(|task| (task.title, task.description))
                .collect();
            for task in &project.tasks {
                if !known.insert((task.title.clone(), task.description.clone())) {
                    report.tasks_skipped += 1;
                    continue;
                }
                let column_id = match columns.iter().find(|c| c.name == task.column) {
                    Some(column) => column.id,
                    None => {
                        let kind = project
                            .columns
                            .iter()
                            .find(|column| column.name == task.column)
                            .map(|column| column.kind);
                        match kind.and_then(|kind| {
                            columns
                                .iter()
                                .find(|column| column.kind == kind && kind != ColumnKind::Custom)
                        }) {
                            Some(column) => column.id,
                            None if task.column.trim().is_empty() => columns[0].id,
                            None => {
                                let created = self.create_column(target.id, &task.column)?;
                                let id = created.id;
                                columns.push(created);
                                id
                            }
                        }
                    }
                };
                let context_id = match task
                    .context
                    .as_deref()
                    .filter(|name| !name.trim().is_empty())
                {
                    Some(name) => Some(
                        match self
                            .list_contexts()?
                            .into_iter()
                            .find(|context| context.name == name)
                        {
                            Some(found) => found.id,
                            None => self.create_context(name)?.id,
                        },
                    ),
                    None => None,
                };
                let mut label_ids = Vec::new();
                for label in &task.labels {
                    let id = match self
                        .list_labels()?
                        .into_iter()
                        .find(|candidate| candidate.name == label.name)
                    {
                        Some(found) => found.id,
                        None => self.create_label(&label.name, &label.colour)?.id,
                    };
                    label_ids.push(id);
                }
                let created = self.create_task(NewTask {
                    project_id: target.id,
                    column_id: Some(column_id),
                    title: task.title.clone(),
                    description: task.description.clone(),
                    due_at: task.due_at,
                    start_at: task.start_at,
                    priority: task.priority.clamp(0, 3),
                    context_id,
                    assignee_email: task.assignee_email.clone(),
                    assignee_name: task.assignee_name.clone(),
                    label_ids,
                    from_mail: false,
                    quick_capture: false,
                })?;
                for item in &task.checklist {
                    let added = self.add_checklist_item(created.id, &item.text)?;
                    if item.done {
                        self.update_checklist_item(added.id, None, Some(true))?;
                    }
                }
                if task.completed_at.is_some() {
                    self.complete_task(created.id)?;
                }
                report.tasks_created += 1;
            }
        }
        Ok(report)
    }
}
