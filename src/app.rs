pub mod actions;
pub mod channels;
pub mod context;
pub mod dialog;
pub mod error;
mod flags;
pub mod markdown;
pub mod menu;

pub use flags::*;
use std::{
    any::TypeId,
    collections::{HashMap, VecDeque},
    env, process,
};

use cli_clipboard::{ClipboardContext, ClipboardProvider};
use cosmic::{
    app::{self, Core},
    cosmic_config::{self, Update},
    cosmic_theme::{self, ThemeMode},
    iced::{
        keyboard::{Event as KeyEvent, Modifiers}, Event, Length, Subscription
    },
    widget::{
        self,
        menu::{key_bind::KeyBind, Action as _},
        segmented_button::{Entity, EntityMut, SingleSelect},
    },
    Application, ApplicationExt, Element,
};
use tracing::info;

use crate::{
    app::{
        actions::{Action, ApplicationAction, NavMenuAction, TasksAction},
        channels::{init_channels, CacheUpdateSignal},
        context::ContextPage,
        dialog::{DateTimeInfo, DialogAction, DialogPage},
    },
    core::{
        config::{self, CONFIG_VERSION},
        icons,
        key_bind::key_binds,
    },
    fl,
    pages::{
        content::{self, Content},
        details::{self, Details},
    },
    storage::{models::List, LocalStorage},
};

pub struct TasksApp {
    core: Core,
    about: widget::about::About,
    nav_model: widget::segmented_button::SingleSelectModel,
    storage: LocalStorage,
    content: Content,
    details: Details,
    config_handler: Option<cosmic_config::Config>,
    config: config::TasksConfig,
    app_themes: Vec<String>,
    context_page: ContextPage,
    key_binds: HashMap<KeyBind, Action>,
    modifiers: Modifiers,
    dialog_pages: VecDeque<DialogPage>,
    dialog_text_input: widget::Id,
    running_operations: u32,
    max_operations: u32,
}

#[derive(Debug, Clone)]
pub enum Message {
    Content(content::Message),
    Details(details::Message),
    Tasks(TasksAction),
    Application(ApplicationAction),
    Open(String),
    OperationStarted,
    OperationCompleted,
    OperationFailed,
    CacheUpdate(CacheUpdateSignal),
    RefreshCacheCounts,
}

impl TasksApp {
    fn settings(&self) -> Element<Message> {
        widget::scrollable(widget::settings::section().title(fl!("appearance")).add(
            widget::settings::item::item(
                fl!("theme"),
                widget::dropdown(
                    &self.app_themes,
                    Some(self.config.app_theme.into()),
                    |theme| Message::Application(ApplicationAction::AppTheme(theme)),
                ),
            ),
        ))
        .into()
    }
    // fn clear_lists(&mut self) {
    //     self.nav_model.clear();
    // }

    fn update_lists(&mut self, lists: Vec<List>) {
        let mut counter = 0;
        for list in lists {
            let mut found = false;
            let mut entity_found: Option<Entity> = None;
            for entity in self.nav_model.iter() {
                if let Some(data) = self.nav_model.data::<List>(entity) {
                    if data.id == list.id {
                        found = true;
                        entity_found = Some(entity);
                    }
                }
            }
            if !found {
                let item = self.create_nav_item(&list);
                if counter == 3 || counter == 5 {
                    item.divider_above(true);
                }
            } else {
                // Create text with task count if there are tasks
                let text = if list.number_of_tasks > 0 {
                    format!("({}) {}", list.number_of_tasks, list.name)
                } else {
                    list.name.clone()
                };
                self.nav_model.text_set(entity_found.unwrap(), text);
                self.nav_model.icon_set(
                    entity_found.unwrap(),
                    crate::app::icons::get_icon(
                        list.icon.as_deref().unwrap_or("view-list-symbolic"),
                        16,
                    ),
                );
            }
            counter += 1;
        }
    }

    fn create_nav_item(&mut self, list: &List) -> EntityMut<SingleSelect> {
        let icon =
            crate::app::icons::get_icon(list.icon.as_deref().unwrap_or("view-list-symbolic"), 16);

        // Create text with task count if there are tasks
        let text = if list.number_of_tasks > 0 {
            format!("({}) {}", list.number_of_tasks, list.name)
        } else {
            list.name.clone()
        };

        self.nav_model
            .insert()
            .text(text)
            .icon(icon)
            .data(list.clone())
    }

    /// Increment running operations counter
    fn start_operation(&mut self) {
        self.running_operations += 1;
        tracing::info!("🔄 Operation started. Total running: {}", self.running_operations);
    }

    /// Decrement running operations counter
    fn complete_operation(&mut self) {
        if self.running_operations > 0 {
            self.running_operations -= 1;
            tracing::info!("✅ Operation completed. Total running: {}", self.running_operations);
        }
    }

    /// Get current operation count
    fn get_operation_count(&self) -> u32 {
        self.running_operations
    }

    fn update_cache_counts(&mut self) {
        let all_tasks = self.storage.task_cache.get_all_tasks();
        
        // Collect entities and their data first to avoid borrow checker issues
        let entities_and_lists: Vec<_> = self.nav_model.iter()
            .filter_map(|entity| {
                self.nav_model.data::<List>(entity).map(|list| (entity, list.clone()))
            })
            .collect();
        
        // Update counts for all lists in nav_model
        for (entity, list) in entities_and_lists {
            let count = if list.is_virtual {
                // Calculate virtual list counts
                match list.virtual_type.as_ref() {
                    Some(crate::storage::models::VirtualListType::MyDay) => {
                        self.storage.filter_my_day_tasks(&all_tasks).len()
                    }
                    Some(crate::storage::models::VirtualListType::Planned) => {
                        self.storage.filter_planned_tasks(&all_tasks).len()
                    }
                    Some(crate::storage::models::VirtualListType::All) => {
                        self.storage.filter_all_tasks(&all_tasks).len()
                    }
                    None => 0,
                }
            } else {
                // Count tasks for regular lists (respect hide_completed setting)
                all_tasks.iter()
                    .filter(|task| {
                        task.list_id.as_ref() == Some(&list.id) &&
                        (list.hide_completed == false || task.status != crate::storage::models::Status::Completed)
                    })
                    .count()
            };
            
            // Update the display text
            let text = if count > 0 {
                format!("({}) {}", count, list.name)
            } else {
                list.name.clone()
            };
            self.nav_model.text_set(entity, text);
        }
    }

    fn update_content(
        &mut self,
        tasks: &mut Vec<cosmic::Task<cosmic::Action<Message>>>,
        message: content::Message,
    ) {
        let content_tasks = self.content.update(message);
        for content_task in content_tasks {
            match content_task {
                content::Output::Focus(id) => {
                    tasks.push(self.update(Message::Application(ApplicationAction::Focus(id))))
                }
                content::Output::FinishedTasksChanged => {
                    tasks.push(self.update(Message::Tasks(TasksAction::FetchLists)));
                }
                content::Output::OpenTaskDetails(task) => {
                    let entity = self.details.priority_model.entity_at(task.priority as u16);
                    if let Some(entity) = entity {
                        self.details.priority_model.activate(entity);
                    }
                    self.details.task = task.clone();
                    self.details.text_editor_content =
                        widget::text_editor::Content::with_text(&task.notes);

                    // Trigger checklist fetch after setting the task
                    if !task.id.is_empty() {
                        tasks.push(self.update(Message::Tasks(
                            TasksAction::FetchChecklistItemsAsync(task.id.clone()),
                        )));
                    }

                    tasks.push(self.update(Message::Application(
                        ApplicationAction::ToggleContextPage(ContextPage::TaskDetails),
                    )));
                }
                content::Output::ToggleHideCompleted(list) => {
                    if let Some(data) = self.nav_model.active_data_mut::<List>() {
                        data.hide_completed = list.hide_completed;
                        // Convert to async operation
                        let mut storage = self.storage.clone();
                        let future = async move { storage.tasks(&list).await };
                        tasks.push(self.spawn_storage_operation(
                            future,
                            |t| Message::Content(content::Message::SetTasks(t)),
                            |error| {
                                tracing::error!("Error updating list hide completed: {}", error);
                                Message::Content(content::Message::Empty)
                            },
                        ));
                    }
                }
                content::Output::CreateTaskAsync(task) => {
                    tasks.push(self.update(Message::Tasks(TasksAction::CreateTaskAsync(task))));
                }
                content::Output::UpdateTaskAsync(task) => {
                    tasks.push(self.update(Message::Tasks(TasksAction::UpdateTaskAsync(task))));
                }
                content::Output::DeleteTaskAsync(task) => {
                    tasks.push(self.update(Message::Tasks(TasksAction::DeleteTaskAsync(task))));
                }
                content::Output::FetchTasksAsync(list) => {
                    tasks.push(self.update(Message::Tasks(TasksAction::FetchTasksAsync(list))));
                }
                content::Output::RefreshListAsync(list) => {
                    tasks.push(self.update(Message::Tasks(TasksAction::RefreshListAsync(list))));
                }
            }
        }
    }

    fn update_details(
        &mut self,
        tasks: &mut Vec<cosmic::Task<cosmic::Action<Message>>>,
        message: details::Message,
    ) {
        let details_tasks = self.details.update(message);
        for details_task in details_tasks {
            match details_task {
                details::Output::OpenCalendarDialog => {
                    tasks.push(self.update(Message::Application(ApplicationAction::Dialog(
                        DialogAction::Open(DialogPage::Calendar(DateTimeInfo::new(
                            chrono::Utc::now().date_naive(),
                        ))),
                    ))));
                }
                details::Output::OpenReminderCalendarDialog => {
                    tasks.push(self.update(Message::Application(ApplicationAction::Dialog(
                        DialogAction::Open(DialogPage::ReminderCalendar(DateTimeInfo::new(
                            chrono::Utc::now().date_naive(),
                        ))),
                    ))));
                }
                details::Output::RefreshTask(task) => {
                    tasks.push(self.update(Message::Content(content::Message::RefreshTask(
                        task.clone(),
                    ))));
                }
                details::Output::UpdateTaskAsync(task) => {
                    tasks.push(self.update(Message::Tasks(TasksAction::UpdateTaskAsync(task))));
                }
                // Handle checklist outputs
                details::Output::AddChecklistItemAsync(title) => {
                    tasks.push(
                        self.update(Message::Tasks(TasksAction::AddChecklistItemAsync(title))),
                    );
                }
                details::Output::UpdateChecklistItemAsync(item_id, new_title) => {
                    tasks.push(
                        self.update(Message::Tasks(TasksAction::UpdateChecklistItemAsync(
                            item_id, new_title,
                        ))),
                    );
                }
                details::Output::ToggleChecklistItemAsync(item_id) => {
                    tasks.push(
                        self.update(Message::Tasks(TasksAction::ToggleChecklistItemAsync(
                            item_id,
                        ))),
                    );
                }
                details::Output::DeleteChecklistItemAsync(item_id) => {
                    tasks.push(
                        self.update(Message::Tasks(TasksAction::DeleteChecklistItemAsync(
                            item_id,
                        ))),
                    );
                }
                details::Output::FetchChecklistItems => {
                    tasks.push(
                        self.update(Message::Tasks(TasksAction::FetchChecklistItemsAsync(
                            self.details.task.id.clone(),
                        ))),
                    );
                }
            }
        }
    }

    /// Helper function to spawn async storage operations
    pub fn spawn_storage_operation<F, T>(
        &mut self,
        future: F,
        success_message: impl FnOnce(T) -> Message + Send + 'static,
        error_message: impl FnOnce(String) -> Message + Send + 'static,
    ) -> cosmic::Task<cosmic::Action<Message>>
    where
        F: std::future::Future<Output = Result<T, crate::Error>> + Send + 'static,
        T: Send + 'static,
    {
        // Note: Operation tracking moved to the actual async action handlers
        // to avoid double-counting when operations call other operations
        
        cosmic::task::future(async move {
            cosmic::Action::App(match future.await {
                Ok(result) => success_message(result),
                Err(error) => error_message(error.to_string()),
            })
        })
    }

    fn update_dialog(
        &mut self,
        tasks: &mut Vec<cosmic::Task<cosmic::Action<Message>>>,
        dialog_action: DialogAction,
    ) {
        match dialog_action {
            DialogAction::Open(page) => {
                match page {
                    DialogPage::Rename(entity, _) => {
                        let data = if let Some(entity) = entity {
                            self.nav_model.data::<List>(entity)
                        } else {
                            self.nav_model.active_data::<List>()
                        };
                        if let Some(list) = data {
                            self.dialog_pages
                                .push_back(DialogPage::Rename(entity, list.name.clone()));
                        }
                    }
                    page => self.dialog_pages.push_back(page),
                }
                tasks.push(self.update(Message::Application(ApplicationAction::Focus(
                    self.dialog_text_input.clone(),
                ))));
            }
            DialogAction::Update(dialog_page) => {
                self.dialog_pages[0] = dialog_page;
            }
            DialogAction::Close => {
                self.dialog_pages.pop_front();
            }
            DialogAction::Complete => {
                if let Some(dialog_page) = self.dialog_pages.pop_front() {
                    match dialog_page {
                        DialogPage::New(name) => {
                            let list = List::new(&name);
                            // Convert to async operation
                            let storage = self.storage.clone();
                            let future = async move { storage.create_list(&list).await };
                            tasks.push(self.spawn_storage_operation(
                                future,
                                |list| Message::Tasks(TasksAction::AddList(list)),
                                |error| {
                                    tracing::error!("Error creating list: {}", error);
                                    Message::Content(content::Message::Empty)
                                },
                            ));
                        }
                        DialogPage::Rename(entity, name) => {
                            let data = if let Some(entity) = entity {
                                self.nav_model.data_mut::<List>(entity)
                            } else {
                                self.nav_model.active_data_mut::<List>()
                            };
                            if let Some(list) = data {
                                list.name.clone_from(&name.clone());
                                let list = list.clone();
                                self.nav_model
                                    .text_set(self.nav_model.active(), name.clone());
                                // Convert to async operation
                                let storage = self.storage.clone();
                                let list_clone = list.clone();
                                let future = async move { storage.update_list(&list_clone).await };
                                tasks.push(self.spawn_storage_operation(
                                    future,
                                    |_| Message::Content(content::Message::Empty),
                                    |error| {
                                        tracing::error!("Error updating list: {}", error);
                                        Message::Content(content::Message::Empty)
                                    },
                                ));
                                tasks.push(self.update(Message::Content(
                                    content::Message::SetList(Some(list)),
                                )));
                            }
                        }
                        DialogPage::Delete(entity) => {
                            tasks
                                .push(self.update(Message::Tasks(TasksAction::DeleteList(entity))));
                        }
                        DialogPage::Calendar(date_time_info) => {
                            self.update_details(
                                tasks,
                                details::Message::SetDueDate(date_time_info.to_naive_datetime()),
                            );
                        }
                        DialogPage::ReminderCalendar(date_time_info) => {
                            self.update_details(
                                tasks,
                                details::Message::SetReminderDate(
                                    date_time_info.to_naive_datetime(),
                                ),
                            );
                        }
                        DialogPage::Export(content) => {
                            let Ok(mut clipboard) = ClipboardContext::new() else {
                                tracing::error!("Clipboard is not available");
                                return;
                            };
                            if let Err(error) = clipboard.set_contents(content) {
                                tracing::error!("Error setting clipboard contents: {error}");
                            }
                        }
                    }
                }
            }
            DialogAction::None => (),
        }
    }

    fn update_app(
        &mut self,
        tasks: &mut Vec<cosmic::Task<cosmic::Action<Message>>>,
        application_action: ApplicationAction,
    ) {
        match application_action {
            ApplicationAction::WindowClose => {
                if let Some(window_id) = self.core.main_window_id() {
                    tasks.push(cosmic::iced::window::close(window_id));
                }
            }
            ApplicationAction::WindowNew => match env::current_exe() {
                Ok(exe) => match process::Command::new(&exe).spawn() {
                    Ok(_) => {}
                    Err(err) => {
                        eprintln!("failed to execute {exe:?}: {err}");
                    }
                },
                Err(err) => {
                    eprintln!("failed to get current executable path: {err}");
                }
            },
            ApplicationAction::AppTheme(theme) => {
                if let Some(handler) = &self.config_handler {
                    if let Err(err) = self.config.set_app_theme(handler, theme.into()) {
                        tracing::error!("{err}")
                    }
                }
            }
            ApplicationAction::ToggleHideCompleted(value) => {
                if let Some(handler) = &self.config_handler {
                    if let Err(err) = self.config.set_hide_completed(handler, value) {
                        tracing::error!("{err}")
                    }
                    tasks.push(self.update(Message::Content(content::Message::SetConfig(
                        self.config.clone(),
                    ))));
                }
            }
            ApplicationAction::SystemThemeModeChange => {
                tasks.push(cosmic::command::set_theme(self.config.app_theme.theme()));
            }
            ApplicationAction::Key(modifiers, key) => {
                for (key_bind, action) in self.key_binds.clone().into_iter() {
                    if key_bind.matches(modifiers, &key) {
                        tasks.push(self.update(action.message()));
                    }
                }
            }
            ApplicationAction::Modifiers(modifiers) => {
                self.modifiers = modifiers;
            }
            ApplicationAction::NavMenuAction(nav_menu_action) => match nav_menu_action {
                NavMenuAction::Rename(entity) => {
                    tasks.push(self.update(Message::Application(ApplicationAction::Dialog(
                        DialogAction::Open(DialogPage::Rename(Some(entity), String::new())),
                    ))));
                }
                NavMenuAction::Export(entity) => {
                    if let Some(list) = self.nav_model.data::<List>(entity) {
                        // Convert to async operation
                        let mut storage = self.storage.clone();
                        let list_clone = list.clone();
                        let future = async move { storage.tasks(&list_clone).await };
                        let list_for_export = list.clone();
                        tasks.push(self.spawn_storage_operation(
                            future,
                            move |data| {
                                let exported_markdown =
                                    LocalStorage::export_list(&list_for_export, &data);
                                Message::Application(ApplicationAction::Dialog(DialogAction::Open(
                                    DialogPage::Export(exported_markdown),
                                )))
                            },
                            move |error| {
                                tracing::error!("Error fetching tasks: {}", error);
                                Message::Content(content::Message::Empty)
                            },
                        ));
                    }
                }
                NavMenuAction::Delete(entity) => {
                    tasks.push(self.update(Message::Application(ApplicationAction::Dialog(
                        DialogAction::Open(DialogPage::Delete(Some(entity))),
                    ))));
                }
            },
            ApplicationAction::ToggleContextPage(context_page) => {
                if self.context_page == context_page {
                    self.core.window.show_context = !self.core.window.show_context;
                } else {
                    self.context_page = context_page;
                    self.core.window.show_context = true;
                }
                tasks.push(
                    self.update(Message::Content(content::Message::ContextMenuOpen(
                        self.core.window.show_context,
                    ))),
                );
            }
            ApplicationAction::ToggleContextDrawer => {
                self.core.window.show_context = !self.core.window.show_context;
                tasks.push(
                    self.update(Message::Content(content::Message::ContextMenuOpen(
                        self.core.window.show_context,
                    ))),
                );
            }
            ApplicationAction::Dialog(dialog_action) => self.update_dialog(tasks, dialog_action),
            ApplicationAction::Focus(id) => tasks.push(widget::text_input::focus(id)),
            ApplicationAction::SortByNameAsc => {
                tasks.push(self.update(Message::Content(content::Message::SetSort(
                    content::SortType::NameAsc,
                ))));
            }
            ApplicationAction::SortByNameDesc => {
                tasks.push(self.update(Message::Content(content::Message::SetSort(
                    content::SortType::NameDesc,
                ))));
            }
            ApplicationAction::SortByDateAsc => {
                tasks.push(self.update(Message::Content(content::Message::SetSort(
                    content::SortType::DateAsc,
                ))));
            }
            ApplicationAction::SortByDateDesc => {
                tasks.push(self.update(Message::Content(content::Message::SetSort(
                    content::SortType::DateDesc,
                ))));
            }
        }
    }

    fn update_tasks(
        &mut self,
        tasks: &mut Vec<cosmic::Task<cosmic::Action<Message>>>,
        tasks_action: TasksAction,
    ) {
        match tasks_action {
            TasksAction::FetchLists => {
                // Convert to async operation
                tasks.push(self.update(Message::Tasks(TasksAction::FetchListsAsync)));
            }
            TasksAction::FetchListsAsync => {
                // Start tracking this operation
                self.start_operation();
                
                let mut storage = self.storage.clone();
                let future = async move { storage.lists().await };
                tasks.push(self.spawn_storage_operation(
                    future,
                    |lists| Message::Tasks(TasksAction::ListsFetched(Ok(lists))),
                    |error| Message::Tasks(TasksAction::ListsFetched(Err(error))),
                ));
            }
            TasksAction::ListsFetched(result) => {
                self.complete_operation();
                match result {
                    Ok(lists) => {
                        self.update_lists(lists);
                        
                        // Spawn a task to refresh cache counts after parallel fetching
                        tasks.push(cosmic::task::future(async move {
                            // Wait a bit for parallel fetching to complete
                            tokio::time::sleep(tokio::time::Duration::from_millis(3000)).await;
                            
                            // Send a message to refresh cache counts
                            cosmic::Action::App(Message::RefreshCacheCounts)
                        }));
                        
                        if self.nav_model.active_data_mut::<List>().is_none() {
                            let Some(entity) = self.nav_model.iter().next() else {
                                return;
                            };
                            self.nav_model.activate(entity);
                            let task = self.on_nav_select(entity);
                            tasks.push(task);
                        }
                    }
                    Err(error) => {
                        tracing::error!("Error fetching lists: {}", error);
                    }
                }
            }
            // TasksAction::PopulateLists(lists) => {
            //     //self.clear_lists();
            //     // for list in lists {
            //     //     self.create_nav_item(&list);
            //     // }
            //     self.update_lists(lists);
            //     let Some(entity) = self.nav_model.iter().next() else {
            //         return;
            //     };
            //     self.nav_model.activate(entity);
            //     let task = self.on_nav_select(entity);
            //     tasks.push(task);
            // }
            TasksAction::AddList(list) => {
                self.create_nav_item(&list);
                let Some(entity) = self.nav_model.iter().last() else {
                    return;
                };
                let task = self.on_nav_select(entity);
                tasks.push(task);
            }
            TasksAction::DeleteList(entity) => {
                let data = if let Some(entity) = entity {
                    self.nav_model.data::<List>(entity)
                } else {
                    self.nav_model.active_data::<List>()
                };
                if let Some(list) = data {
                    // Convert to async operation
                    tasks.push(
                        self.update(Message::Tasks(TasksAction::DeleteListAsync(list.clone()))),
                    );
                }
                self.nav_model.remove(self.nav_model.active());
            }
            TasksAction::CreateTaskAsync(task) => {
                // Start tracking this operation
                self.start_operation();
                
                let mut storage = self.storage.clone();

                // Handle virtual list task routing
                let mut task_to_create = task.clone();
                if let Some(current_list) = self.nav_model.active_data::<List>() {
                    if current_list.is_virtual {
                        // Find the "Tasks" list (well_known_list_name = "defaultList")
                        for entity in self.nav_model.iter() {
                            if let Some(list) = self.nav_model.data::<List>(entity) {
                                if list.well_known_list_name.as_deref() == Some("defaultList") {
                                    task_to_create.list_id = Some(list.id.clone());
                                    break;
                                }
                            }
                        }
                    }
                }

                let future = async move { storage.create_task(&task_to_create).await };
                tasks.push(self.spawn_storage_operation(
                    future,
                    |task| Message::Tasks(TasksAction::TaskCreated(Ok(task))),
                    |error| Message::Tasks(TasksAction::TaskCreated(Err(error))),
                ));
            }
            TasksAction::TaskCreated(result) => {
                self.complete_operation();
                match result {
                    Ok(task) => {
                        // Handle successful task creation
                        tasks.push(
                            self.update(Message::Content(content::Message::TaskCreated(task))),
                        );
                        // Refresh cache counts
                        tasks.push(self.update(Message::RefreshCacheCounts));
                    }
                    Err(error) => {
                        tracing::error!("Failed to create task: {}", error);
                    }
                }
            }
            TasksAction::UpdateTaskAsync(task) => {
                // Start tracking this operation
                self.start_operation();
                
                let mut storage = self.storage.clone();
                let future = async move { storage.update_task(&task).await };
                tasks.push(self.spawn_storage_operation(
                    future,
                    |_| Message::Tasks(TasksAction::TaskUpdated(Ok(()))),
                    |error| Message::Tasks(TasksAction::TaskUpdated(Err(error))),
                ));
            }
            TasksAction::TaskUpdated(result) => {
                self.complete_operation();
                match result {
                    Ok(_) => {
                        // Task updated successfully
                        tracing::info!("Task updated successfully");
                        if let Some(current_list) = self.nav_model.active_data::<List>() {
                            tasks.push(self.update(Message::Tasks(TasksAction::FetchTasksAsync(
                                current_list.clone(),
                            ))));
                        }
                        // Refresh cache counts
                        tasks.push(self.update(Message::RefreshCacheCounts));
                    }
                    Err(error) => {
                        tracing::error!("Failed to update task: {}", error);
                    }
                }
            }
            TasksAction::DeleteTaskAsync(task) => {
                // Start tracking this operation
                self.start_operation();
                
                let mut storage = self.storage.clone();
                let future = async move { storage.delete_task(&task).await };
                tasks.push(self.spawn_storage_operation(
                    future,
                    |_| Message::Tasks(TasksAction::TaskDeleted(Ok(()))),
                    |error| Message::Tasks(TasksAction::TaskDeleted(Err(error))),
                ));
            }
            TasksAction::TaskDeleted(result) => {
                self.complete_operation();
                match result {
                    Ok(_) => {
                        // Task deleted successfully
                        tracing::info!("Task deleted successfully");
                        // Refresh cache counts
                        tasks.push(self.update(Message::RefreshCacheCounts));
                    }
                    Err(error) => {
                        tracing::error!("Failed to delete task: {}", error);
                    }
                }
            }
            TasksAction::DeleteListAsync(list) => {
                // Start tracking this operation
                self.start_operation();
                
                let storage = self.storage.clone();
                let future = async move { storage.delete_list(&list).await };
                tasks.push(self.spawn_storage_operation(
                    future,
                    |_| Message::Tasks(TasksAction::ListDeleted(Ok(()))),
                    |error| Message::Tasks(TasksAction::ListDeleted(Err(error))),
                ));
            }
            TasksAction::ListDeleted(result) => {
                self.complete_operation();
                match result {
                    Ok(_) => {
                        tracing::info!("List deleted successfully");
                    }
                    Err(error) => {
                        tracing::error!("Failed to delete list: {}", error);
                    }
                }
            }

            // NEW: Add these cases
            TasksAction::FetchTasksAsync(list) => {
                // Start tracking this operation
                self.start_operation();
                
                let mut storage = self.storage.clone();
                let future = async move { storage.tasks(&list).await };
                tasks.push(self.spawn_storage_operation(
                    future,
                    |tasks| Message::Tasks(TasksAction::TasksFetched(Ok(tasks))),
                    |error| Message::Tasks(TasksAction::TasksFetched(Err(error))),
                ));
            }

            TasksAction::RefreshListAsync(list) => {
                // Start tracking this operation
                self.start_operation();
                
                let mut storage = self.storage.clone();
                let future = async move { storage.refresh_tasks(&list).await };
                tasks.push(self.spawn_storage_operation(
                    future,
                    |tasks| Message::Tasks(TasksAction::TasksFetched(Ok(tasks))),
                    |error| Message::Tasks(TasksAction::TasksFetched(Err(error))),
                ));
            }

            TasksAction::TasksFetched(result) => {
                self.complete_operation();
                match result {
                    Ok(task_list) => {
                        // Send tasks to content page
                        tasks.push(
                            self.update(Message::Content(content::Message::SetTasks(task_list))),
                        );
                    }
                    Err(error) => {
                        tracing::error!("Failed to fetch tasks: {}", error);
                    }
                }
            }
            // Handle checklist actions
            TasksAction::AddChecklistItemAsync(title) => {
                // Start tracking this operation
                self.start_operation();
                
                // Get current task from details
                let task = self.details.task.clone();
                let storage = self.storage.clone();
                let future = async move {
                    // Create checklist item via MS Graph
                    storage.create_checklist_item(&task, &title).await
                };
                tasks.push(self.spawn_storage_operation(
                    future,
                    |item| Message::Tasks(TasksAction::ChecklistItemAdded(Ok(item))),
                    |error| Message::Tasks(TasksAction::ChecklistItemAdded(Err(error.to_string()))),
                ));
                // Clear the input immediately for better UX
                self.details.new_checklist_item_text.clear();
            }
            TasksAction::ChecklistItemAdded(result) => {
                self.complete_operation();
                match result {
                    Ok(item) => {
                        tracing::info!("Checklist item added successfully");
                        // Add the new item to the local task and refresh
                        let mut task = self.details.task.clone();
                        task.checklist_items.push(item);
                        task.checklist_sync_status =
                            crate::storage::models::task::ChecklistSyncStatus::Synced;
                        self.details.task = task;
                    }
                    Err(error) => {
                        tracing::error!("Failed to add checklist item: {}", error);
                        // Mark sync as failed
                        let mut task = self.details.task.clone();
                        task.checklist_sync_status =
                            crate::storage::models::task::ChecklistSyncStatus::SyncFailed(error);
                        self.details.task = task;
                    }
                }
            }
            TasksAction::UpdateChecklistItemAsync(item_id, new_title) => {
                let task = self.details.task.clone();
                let storage = self.storage.clone();
                let future = async move {
                    // Get current checklist item to preserve checked state
                    let current_item = task
                        .checklist_items
                        .iter()
                        .find(|item| item.id == item_id)
                        .cloned();

                    if let Some(item) = current_item {
                        // Update via MS Graph API
                        storage
                            .update_checklist_item(&task, &item_id, &new_title, item.is_checked)
                            .await
                    } else {
                        Err(crate::app::error::Error::Tasks(
                            crate::app::error::TasksError::TaskNotFound,
                        ))
                    }
                };
                tasks.push(self.spawn_storage_operation(
                    future,
                    |updated_item| {
                        Message::Tasks(TasksAction::ChecklistItemUpdated(Ok(updated_item)))
                    },
                    |error| {
                        Message::Tasks(TasksAction::ChecklistItemUpdated(Err(error.to_string())))
                    },
                ));
            }
            TasksAction::ChecklistItemUpdated(result) => {
                self.complete_operation();
                match result {
                    Ok(updated_item) => {
                        tracing::info!("Checklist item updated successfully");
                        // Update the local task with the updated item
                        let mut task = self.details.task.clone();
                        if let Some(item) = task
                            .checklist_items
                            .iter_mut()
                            .find(|item| item.id == updated_item.id)
                        {
                            *item = updated_item;
                        }
                        task.checklist_sync_status =
                            crate::storage::models::task::ChecklistSyncStatus::Synced;
                        self.details.task = task;
                    }
                    Err(error) => {
                        tracing::error!("Failed to update checklist item: {}", error);
                        // Mark sync as failed
                        let mut task = self.details.task.clone();
                        task.checklist_sync_status =
                            crate::storage::models::task::ChecklistSyncStatus::SyncFailed(error);
                        self.details.task = task;
                    }
                }
            }
            TasksAction::ToggleChecklistItemAsync(item_id) => {
                let task = self.details.task.clone();
                let storage = self.storage.clone();
                let future = async move {
                    // Get current checklist item to toggle checked state
                    let current_item = task
                        .checklist_items
                        .iter()
                        .find(|item| item.id == item_id)
                        .cloned();

                    if let Some(item) = current_item {
                        // Toggle via MS Graph API
                        let new_checked_state = !item.is_checked;
                        storage
                            .update_checklist_item(
                                &task,
                                &item_id,
                                &item.display_name,
                                new_checked_state,
                            )
                            .await
                    } else {
                        Err(crate::app::error::Error::Tasks(
                            crate::app::error::TasksError::TaskNotFound,
                        ))
                    }
                };
                tasks.push(self.spawn_storage_operation(
                    future,
                    |updated_item| {
                        Message::Tasks(TasksAction::ChecklistItemToggled(Ok(updated_item)))
                    },
                    |error| {
                        Message::Tasks(TasksAction::ChecklistItemToggled(Err(error.to_string())))
                    },
                ));
            }
            TasksAction::ChecklistItemToggled(result) => {
                match result {
                    Ok(updated_item) => {
                        tracing::info!("Checklist item toggled successfully");
                        // Update the local task with the updated item
                        let mut task = self.details.task.clone();
                        if let Some(item) = task
                            .checklist_items
                            .iter_mut()
                            .find(|item| item.id == updated_item.id)
                        {
                            *item = updated_item;
                        }
                        task.checklist_sync_status =
                            crate::storage::models::task::ChecklistSyncStatus::Synced;
                        self.details.task = task;
                    }
                    Err(error) => {
                        tracing::error!("Failed to toggle checklist item: {}", error);
                        // Mark sync as failed
                        let mut task = self.details.task.clone();
                        task.checklist_sync_status =
                            crate::storage::models::task::ChecklistSyncStatus::SyncFailed(error);
                        self.details.task = task;
                    }
                }
            }
            TasksAction::DeleteChecklistItemAsync(item_id) => {
                let task = self.details.task.clone();
                let storage = self.storage.clone();
                let future = async move {
                    // Delete via MS Graph API
                    storage.delete_checklist_item(&task, &item_id).await
                };
                tasks.push(self.spawn_storage_operation(
                    future,
                    |item_id_recived| {
                        Message::Tasks(TasksAction::ChecklistItemDeleted(Ok(item_id_recived)))
                    },
                    |error| {
                        Message::Tasks(TasksAction::ChecklistItemDeleted(Err(error.to_string())))
                    },
                ));
            }
            TasksAction::ChecklistItemDeleted(result) => {
                match result {
                    Ok(item_id_recived) => {
                        tracing::info!("Checklist item deleted successfully");
                        // Remove the item from local task immediately for better UX
                        // We need to find which item was deleted by comparing with the current state
                        // For now, we'll refresh the checklist items to ensure consistency
                        if !self.details.task.id.is_empty() {

                            //task.checklist_items = task.checklist_items.iter().filter(|item| item.id != item_id_recived).cloned().collect();
                        }
                        let mut task = self.details.task.clone();
                        task.checklist_items = task
                            .checklist_items
                            .iter()
                            .filter(|item| item.id != item_id_recived)
                            .cloned()
                            .collect();
                        task.checklist_sync_status =
                            crate::storage::models::task::ChecklistSyncStatus::Synced;
                        self.details.task = task;
                    }
                    Err(error) => {
                        tracing::error!("Failed to delete checklist item: {}", error);
                        // Mark sync as failed
                        let mut task = self.details.task.clone();
                        task.checklist_sync_status =
                            crate::storage::models::task::ChecklistSyncStatus::SyncFailed(error);
                        self.details.task = task;
                    }
                }
            }
            TasksAction::FetchChecklistItemsAsync(task_id) => {
                // Start tracking this operation
                self.start_operation();
                
                tracing::info!("🔄 Fetching checklist items for task: {}", task_id);
                let task = self.details.task.clone();
                let storage = self.storage.clone();
                let future = async move {
                    // Fetch checklist items via MS Graph API
                    storage.fetch_checklist_items(&task).await
                };
                let task_id_clone = task_id.clone();
                tasks.push(self.spawn_storage_operation(
                    future,
                    move |items| {
                        tracing::info!(
                            "✅ Fetched {} checklist items for task: {}",
                            items.len(),
                            task_id_clone
                        );
                        Message::Tasks(TasksAction::ChecklistItemsFetched(Ok(items)))
                    },
                    move |error| {
                        tracing::error!(
                            "❌ Failed to fetch checklist items for task {}: {}",
                            task_id,
                            error
                        );
                        Message::Tasks(TasksAction::ChecklistItemsFetched(Err(error)))
                    },
                ));
            }
            TasksAction::ChecklistItemsFetched(result) => {
                self.complete_operation();
                match result {
                    Ok(items) => {
                        tracing::info!("✅ Successfully fetched {} checklist items", items.len());
                        // Update the task with fetched checklist items
                        let mut task = self.details.task.clone();
                        task.checklist_items = items.clone();
                        task.checklist_sync_status =
                            crate::storage::models::task::ChecklistSyncStatus::Synced;
                        self.details.task = task;
                        tracing::info!(
                            "🔄 Updated local task with {} checklist items",
                            items.len()
                        );
                    }
                    Err(error) => {
                        tracing::error!("❌ Failed to fetch checklist items: {}", error);
                        // Mark sync as failed
                        let mut task = self.details.task.clone();
                        task.checklist_sync_status =
                            crate::storage::models::task::ChecklistSyncStatus::SyncFailed(error);
                        self.details.task = task;
                    }
                }
            } // Handle any other actions
              // _ => {
              //     tracing::debug!("Unhandled action: {:?}", tasks_action);
              // }
        }
    }
}

impl Application for TasksApp {
    type Executor = cosmic::executor::Default;
    type Flags = crate::app::Flags;
    type Message = Message;
    const APP_ID: &'static str = "com.github.digit1024.ms-todo-app";

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, flags: Self::Flags) -> (Self, app::Task<Self::Message>) {
        // Initialize channels first
        let _channel_manager = init_channels();
        
        let nav_model = widget::segmented_button::ModelBuilder::default().build();

        let about = widget::about::About::default()
            .name(fl!("tasks"))
            .icon(cosmic::widget::icon::Named::new(Self::APP_ID))
            .version("1.0.0")
            .author("Eduardo Flores (original) and Mchał Banaś (microsoft TODO)")
            .license("GPL-3.0-only")
            .links([
                (fl!("repository-original"), "https://github.com/cosmic-utils/tasks"),
                (fl!("repository-microsoft-todo"), "https://github.com/digit1024/msToDO"),
                (
                    fl!("support-original"),
                    "https://github.com/cosmic-utils/tasks/issues",
                ),
                (fl!("support"), "https://github.com/digit1024/msToDO/issues"),
                (fl!("website-original"), "https://tasks.edfloreshz.dev"),
                (fl!("website-microsoft-todo"), "https://github.com/digit1024/msToDO"),
            ]).comments("I want to thank the original author of 'tasks'. Without his work, this app would not be possible.")
            .developers([("Eduardo Flores", "edfloreshz@proton.me") ,("Michał Banaś", "https://github.com/digit1024")]);

        let mut app = TasksApp {
            core,
            about,
            storage: flags.storage.clone(),
            nav_model,
            content: Content::new(),
            details: Details::new(),
            config_handler: flags.config_handler,
            config: flags.config,
            app_themes: vec![fl!("match-desktop"), fl!("dark"), fl!("light")],
            context_page: ContextPage::Settings,
            key_binds: key_binds(),
            modifiers: Modifiers::empty(),
            dialog_pages: VecDeque::new(),
            dialog_text_input: widget::Id::unique(),
            running_operations: 0,
            max_operations: 1,
        };

        let mut tasks = vec![app.update(Message::Tasks(TasksAction::FetchLists))];

        if let Some(id) = app.core.main_window_id() {
            tasks.push(app.set_window_title(fl!("tasks"), id));
        }

        app.core.nav_bar_toggle_condensed();

        (app, app::Task::batch(tasks))
    }

    fn context_drawer(&self) -> Option<app::context_drawer::ContextDrawer<Self::Message>> {
        if !self.core.window.show_context {
            return None;
        }

        Some(match self.context_page {
            ContextPage::About => app::context_drawer::about(
                &self.about,
                Message::Open,
                Message::Application(ApplicationAction::ToggleContextDrawer),
            )
            .title(self.context_page.title()),
            ContextPage::Settings => app::context_drawer::context_drawer(
                self.settings(),
                Message::Application(ApplicationAction::ToggleContextDrawer),
            )
            .title(self.context_page.title()),
            ContextPage::TaskDetails => app::context_drawer::context_drawer(
                self.details.view().map(Message::Details),
                Message::Application(ApplicationAction::ToggleContextDrawer),
            )
            .title(self.context_page.title()),
        })
    }

    fn dialog(&self) -> Option<Element<Message>> {
        let dialog_page = self.dialog_pages.front()?;
        let dialog = dialog_page.view(&self.dialog_text_input);
        Some(dialog.into())
    }

    fn footer(&self) -> Option<Element<Message>> {
        tracing::debug!("Footer check: running_operations = {}", self.running_operations);
        if self.running_operations > 0 {
            let progress = if self.max_operations > 0 {
                (self.running_operations as f32) / (self.max_operations as f32)
            } else {
                0.5 // Indeterminate progress
            };
            
            tracing::info!("📊 Showing footer with {} operations, progress: {:.2}", self.running_operations, progress);
            
            Some(
                widget::container(
                    widget::column::with_capacity(2)
                        .push(widget::text::body("Syncing with Microsoft Todo..."))
                        .push(
                            widget::progress_bar(0.0..=1.0, progress)
                                .width(Length::Fill)
                        )
                        .spacing(8)
                )
                .padding(12)
                .into()
            )
        } else {
            None
        }
    }

    fn header_start(&self) -> Vec<Element<Self::Message>> {
        vec![menu::menu_bar(&self.key_binds, &self.config)]
    }

    fn nav_context_menu(
        &self,
        id: widget::nav_bar::Id,
    ) -> Option<Vec<widget::menu::Tree<cosmic::Action<Self::Message>>>> {
        Some(cosmic::widget::menu::items(
            &HashMap::new(),
            vec![
                cosmic::widget::menu::Item::Button(
                    fl!("rename"),
                    Some(icons::get_handle("edit-symbolic", 14)),
                    NavMenuAction::Rename(id),
                ),
                cosmic::widget::menu::Item::Button(
                    fl!("export"),
                    Some(icons::get_handle("share-symbolic", 18)),
                    NavMenuAction::Export(id),
                ),
                cosmic::widget::menu::Item::Button(
                    fl!("delete"),
                    Some(icons::get_handle("user-trash-full-symbolic", 14)),
                    NavMenuAction::Delete(id),
                ),
            ],
        ))
    }

    fn nav_model(&self) -> Option<&widget::segmented_button::SingleSelectModel> {
        Some(&self.nav_model)
    }

    fn on_escape(&mut self) -> app::Task<Self::Message> {
        if self.dialog_pages.pop_front().is_some() {
            return app::Task::none();
        }

        self.core.window.show_context = false;

        app::Task::none()
    }

    fn on_nav_select(&mut self, entity: Entity) -> app::Task<Self::Message> {
        let mut tasks = vec![];
        self.nav_model.activate(entity);
        let location_opt = self.nav_model.data::<List>(entity);

        if let Some(list) = location_opt {
            let message = Message::Content(content::Message::SetList(Some(list.clone())));
            let window_title = format!("{} - {}", list.name, fl!("tasks"));
            if let Some(window_id) = self.core.main_window_id() {
                tasks.push(self.set_window_title(window_title, window_id));
            }
            return self.update(message);
        }

        app::Task::batch(tasks)
    }


    fn subscription(&self) -> Subscription<Self::Message> {
        struct ConfigSubscription;
        struct ThemeSubscription;

        let mut subscriptions = vec![
            cosmic::iced::event::listen_with(|event, _status, _window_id| match event {
                Event::Keyboard(KeyEvent::KeyPressed { key, modifiers, .. }) => {
                    Some(Message::Application(ApplicationAction::Key(modifiers, key)))
                }
                Event::Keyboard(KeyEvent::ModifiersChanged(modifiers)) => Some(
                    Message::Application(ApplicationAction::Modifiers(modifiers)),
                ),
                _ => None,
            }),
            cosmic_config::config_subscription(
                TypeId::of::<ConfigSubscription>(),
                Self::APP_ID.into(),
                CONFIG_VERSION,
            )
            .map(|update: Update<ThemeMode>| {
                if !update.errors.is_empty() {
                    tracing::info!(
                        "errors loading config {:?}: {:?}",
                        update.keys,
                        update.errors
                    );
                }
                Message::Application(ApplicationAction::SystemThemeModeChange)
            }),
            cosmic_config::config_subscription::<_, cosmic_theme::ThemeMode>(
                TypeId::of::<ThemeSubscription>(),
                cosmic_theme::THEME_MODE_ID.into(),
                cosmic_theme::ThemeMode::version(),
            )
            .map(|update: Update<ThemeMode>| {
                if !update.errors.is_empty() {
                    tracing::info!(
                        "errors loading theme mode {:?}: {:?}",
                        update.keys,
                        update.errors
                    );
                }
                Message::Application(ApplicationAction::SystemThemeModeChange)
            }),
        ];

        subscriptions.push(self.content.subscription().map(Message::Content));

        Subscription::batch(subscriptions)
    }

    fn update(&mut self, message: Self::Message) -> app::Task<Self::Message> {
        let mut tasks = vec![];
        match message {
            Message::Open(url) => {
                if let Err(err) = open::that_detached(url) {
                    tracing::error!("{err}")
                }
            }
            //for
            Message::Content(message) => {
                self.update_content(&mut tasks, message);
            }
            Message::Details(message) => {
                self.update_details(&mut tasks, message);
            }
            Message::Tasks(tasks_action) => {
                self.update_tasks(&mut tasks, tasks_action);
            }
            Message::Application(application_action) => {
                self.update_app(&mut tasks, application_action);
            }
            Message::OperationStarted => {
                self.start_operation();
            }
            Message::OperationCompleted => {
                self.complete_operation();
            }
            Message::OperationFailed => {
                self.complete_operation();
            }
            Message::CacheUpdate(signal) => {
                // Handle cache update signals
                match signal {
                    CacheUpdateSignal::ListTasksUpdated(_list_id) => {
                        info!("🔄 Cache updated for list, updating UI counts");
                        self.update_cache_counts();
                    }
                    CacheUpdateSignal::TaskUpdated(_, _) => {
                        info!("🔄 Task updated, updating UI counts");
                        self.update_cache_counts();
                    }
                    CacheUpdateSignal::TaskAdded(_, _) => {
                        info!("🔄 Task added, updating UI counts");
                        self.update_cache_counts();
                    }
                    CacheUpdateSignal::TaskRemoved(_, _) => {
                        info!("🔄 Task removed, updating UI counts");
                        self.update_cache_counts();
                    }
                }
            }
            Message::RefreshCacheCounts => {
                info!("🔄 Refreshing cache counts");
                self.update_cache_counts();
            }
        }

        app::Task::batch(tasks)
    }

    fn view(&self) -> Element<Self::Message> {
        self.content.view().map(Message::Content)
    }
}
