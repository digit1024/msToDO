pub mod models;
pub mod cache;

use crate::{
    app::markdown::Markdown,
    app::channels::{get_channel_manager, CacheUpdateSignal},
    storage::models::{List, Task, VirtualListType},
    storage::cache::TaskCache,
    Error, LocalStorageError, TasksError,
};

use tracing::{debug, error, info};

use crate::integration::ms_todo::{http_client::MsTodoHttpClient, models::*};

use crate::auth::ms_todo_auth::MsTodoAuth;

#[derive(Debug, Clone)]
pub struct LocalStorage {
    http_client: MsTodoHttpClient,
    auth: MsTodoAuth,
    pub task_cache: TaskCache,
}


impl LocalStorage {
    pub fn new(_application_id: &str) -> Result<Self, LocalStorageError> {
        // For MS Graph, we need to get the auth token
        let auth = MsTodoAuth::new().map_err(|e| {
            LocalStorageError::LocalStorageDirectoryCreationFailed(std::io::Error::new(
                std::io::ErrorKind::Other,
                e.to_string(),
            ))
        })?;

        // Don't check has_valid_tokens() here - let get_access_token() handle validation and refresh
        // This allows automatic token refresh to work on each API call

        Ok(Self {
            http_client: MsTodoHttpClient::new(),
            auth: auth,
            task_cache: TaskCache::new(),
        })
    }

    /// Get a valid access token, refreshing if necessary
    fn get_valid_token(&self) -> Result<String, LocalStorageError> {
        self.auth.get_access_token().map_err(|e| {
            LocalStorageError::LocalStorageDirectoryCreationFailed(std::io::Error::new(
                std::io::ErrorKind::Other,
                e.to_string(),
            ))
        })
    }

    /// Send cache update signal using global channel manager
    fn send_cache_signal(&self, signal: CacheUpdateSignal) {
        get_channel_manager().send_cache_update(signal);
    }

    // Cache management methods
    async fn add_task_to_cache(&mut self, task: Task) -> Result<(), Error> {
        self.task_cache.update_task(task.clone());
        self.send_cache_signal(CacheUpdateSignal::TaskAdded(
            task.id.clone(), 
            task.list_id.clone().unwrap_or_default()
        ));
        Ok(())
    }
    
    async fn update_task_in_cache(&mut self, task: Task) -> Result<(), Error> {
        self.task_cache.update_task(task.clone());
        self.send_cache_signal(CacheUpdateSignal::TaskUpdated(
            task.id.clone(), 
            task.list_id.clone().unwrap_or_default()
        ));
        Ok(())
    }
    
    async fn remove_task_from_cache(&mut self, task_id: &str, list_id: &str) -> Result<(), Error> {
        self.task_cache.remove_task(task_id, list_id);
        self.send_cache_signal(CacheUpdateSignal::TaskRemoved(
            task_id.to_string(), 
            list_id.to_string()
        ));
        Ok(())
    }
    
    async fn update_cache_for_list(&mut self, list_id: &str, tasks: Vec<Task>) -> Result<(), Error> {
        self.task_cache.update_list_tasks(list_id, tasks);
        self.send_cache_signal(CacheUpdateSignal::ListTasksUpdated(list_id.to_string()));
        Ok(())
    }

    async fn get_tasks_for_virtual_list(&self, list: &List) -> Result<Vec<Task>, Error> {
        let virtual_type = list.virtual_type.as_ref()
            .ok_or_else(|| Error::Tasks(TasksError::TaskNotFound))?;
        
        let all_tasks = self.task_cache.get_all_tasks();
        info!("🔍 Getting tasks for virtual list: {:?} ({} total tasks in cache)", 
              virtual_type, all_tasks.len());
        
        // Use cached data for virtual lists
        let tasks = match virtual_type {
            VirtualListType::MyDay => self.filter_my_day_tasks(&all_tasks),
            VirtualListType::Planned => self.filter_planned_tasks(&all_tasks),
            VirtualListType::All => self.filter_all_tasks(&all_tasks),
        };
        
        info!("✅ Virtual list {:?} has {} tasks", virtual_type, tasks.len());
        
        Ok(tasks)
    }
    
    pub fn filter_my_day_tasks(&self, tasks: &[Task]) -> Vec<Task> {
        let today = chrono::Utc::now().date_naive();
        tasks.iter()
            .filter(|task| {
                task.status != crate::storage::models::Status::Completed &&
                task.due_date.map(|d| d.date_naive() == today).unwrap_or(false)
            })
            .cloned()
            .collect()
    }
    
    pub fn filter_planned_tasks(&self, tasks: &[Task]) -> Vec<Task> {
        tasks.iter()
            .filter(|task| {
                task.status != crate::storage::models::Status::Completed &&
                task.due_date.is_some()
            })
            .cloned()
            .collect()
    }
    
    pub fn filter_all_tasks(&self, tasks: &[Task]) -> Vec<Task> {
        tasks.iter()
            .filter(|task| task.status != crate::storage::models::Status::Completed)
            .cloned()
            .collect()
    }

    async fn initialize_cache(&mut self) -> Result<(), Error> {
        info!("🔄 Initializing task cache...");
        
        // Fetch all lists
        let lists = self.fetch_lists_via_api().await?;
        info!("📋 Found {} lists", lists.len());
        
        // Fetch tasks for each list and populate cache
        for list in lists {
            if !list.is_virtual {
                info!("📝 Fetching tasks for list: {}", list.name);
                let tasks = self.get_active_tasks(&list).await?;
                info!("✅ Found {} tasks for list: {}", tasks.len(), list.name);
                self.task_cache.update_list_tasks(&list.id, tasks);
            }
        }
        
        let all_tasks = self.task_cache.get_all_tasks();
        info!("🎯 Cache initialized with {} total tasks", all_tasks.len());
        
        Ok(())
    }

    async fn fetch_lists_via_api(&self) -> Result<Vec<List>, Error> {
        let auth_token = self
            .get_valid_token()
            .map_err(|_e| Error::Tasks(TasksError::ListNotFound))?;

        // Use helper method to fetch all pages with pagination
        let todo_lists = self.fetch_all_todo_lists(&auth_token).await?;

        let mut lists = Vec::new();
        for tl in todo_lists {
            let l: List = tl.into();
            lists.push(l);
        }
        
        Ok(lists)
    }

    pub async fn tasks(&mut self, list: &List) -> Result<Vec<Task>, Error> {
        // For virtual lists, use cache
        if list.is_virtual {
            return self.get_tasks_for_virtual_list(list).await;
        }
        
        // For regular lists, try cache first
        let all_tasks = self.task_cache.get_all_tasks();
        let mut cached_tasks: Vec<Task> = all_tasks
            .into_iter()
            .filter(|task| task.list_id.as_ref() == Some(&list.id))
            .collect();
        
        // Apply hide_completed filter to cached tasks (consistent with API behavior)
        if list.hide_completed {
            cached_tasks.retain(|task| task.status != crate::storage::models::Status::Completed);
        }
        
        // If we have cached tasks, return them immediately
        if !cached_tasks.is_empty() {
            info!("📋 Using cached tasks for list: {} ({} tasks, hide_completed: {})", 
                  list.name, cached_tasks.len(), list.hide_completed);
            return Ok(cached_tasks);
        }
        
        // If no cached tasks, fetch from API
        info!("🌐 No cached tasks found, fetching from API for list: {}", list.name);
        self.refresh_tasks(list).await
    }

    /// Force refresh tasks from API (bypasses cache)
    pub async fn refresh_tasks(&mut self, list: &List) -> Result<Vec<Task>, Error> {
        // For virtual lists, use cache (they don't have API endpoints)
        if list.is_virtual {
            return self.get_tasks_for_virtual_list(list).await;
        }
        
        info!("🔄 Force refreshing tasks from API for list: {}", list.name);
        
        let auth_token = self
            .get_valid_token()
            .map_err(|_e| Error::Tasks(TasksError::TaskNotFound))?;
        let url =if list.hide_completed {
            format!("/me/todo/lists/{}/tasks?$filter=status ne 'completed'&$orderby=createdDateTime desc", list.id)
        } else {
            format!("/me/todo/lists/{}/tasks?$orderby=createdDateTime desc", list.id)
        };

        // Use helper method to fetch all pages with pagination
        let todo_tasks = self.fetch_all_todo_tasks(&url, &auth_token).await?;

        // Convert TodoTask[] → Task[] with proper path construction
        let tasks: Vec<Task> = todo_tasks
            .into_iter()
            .map(|todo_task| {
                crate::integration::ms_todo::mapping::todo_task_to_task_with_path(
                    todo_task, &list.id,
                )
            })
            .collect();

        // Update cache for this list
        self.update_cache_for_list(&list.id, tasks.clone()).await?;

        info!("✅ Refreshed {} tasks from API for list: {}", tasks.len(), list.name);
        Ok(tasks)
    }
    pub async fn get_active_tasks(&self, list: &List) -> Result<Vec<Task>, Error> {
        let auth_token = self
            .get_valid_token()
            .map_err(|_e| Error::Tasks(TasksError::TaskNotFound))?;

        // Use helper method to fetch all pages with pagination
        let url = format!("/me/todo/lists/{}/tasks?$filter=status ne 'completed'", list.id);
        let todo_tasks = self.fetch_all_todo_tasks(&url, &auth_token).await?;

        // Convert TodoTask[] → Task[] with proper path construction
        let tasks: Vec<Task> = todo_tasks
            .into_iter()
            .map(|todo_task| {
                crate::integration::ms_todo::mapping::todo_task_to_task_with_path(
                    todo_task, &list.id,
                )
            })
            .collect();

        Ok(tasks)
    }

    /// Fetch all tasks for a list (including completed ones)
    async fn get_all_tasks_for_list(&self, list: &List) -> Result<Vec<Task>, Error> {
        let auth_token = self
            .get_valid_token()
            .map_err(|_e| Error::Tasks(TasksError::TaskNotFound))?;

        let url = format!("/me/todo/lists/{}/tasks?$orderby=createdDateTime desc", list.id);
        let todo_tasks = self.fetch_all_todo_tasks(&url, &auth_token).await?;

        // Convert TodoTask[] → Task[] with proper path construction
        let tasks: Vec<Task> = todo_tasks
            .into_iter()
            .map(|todo_task| {
                crate::integration::ms_todo::mapping::todo_task_to_task_with_path(
                    todo_task, &list.id,
                )
            })
            .collect();

        Ok(tasks)
    }

    /// Spawn parallel task fetching for all lists
    pub async fn spawn_parallel_task_fetching(&self, lists: Vec<List>) -> Result<(), Error> {
        let storage = self.clone();
        
        tokio::spawn(async move {
            let mut handles = Vec::new();
            let mut delay_counter = 0;
            
            for list in lists {
                if !list.is_virtual {
                    let storage_clone = storage.clone();
                    let list_clone = list.clone();
                    
                    // Add a small delay between starting requests to avoid rate limiting
                    if delay_counter > 0 {
                        let jitter_delay = std::time::Duration::from_millis(100 * delay_counter);
                        tokio::time::sleep(jitter_delay).await;
                    }
                    delay_counter += 1;
                    
                    let handle = tokio::spawn(async move {
                        match storage_clone.get_all_tasks_for_list(&list_clone).await {
                            Ok(tasks) => {
                                info!("✅ Fetched {} tasks for list: {}", tasks.len(), list_clone.name);
                                
                                // Update cache
                                let mut cache = storage_clone.task_cache.clone();
                                cache.update_list_tasks(&list_clone.id, tasks.clone());
                                
                                // Send signal for UI update
                                storage_clone.send_cache_signal(CacheUpdateSignal::ListTasksUpdated(list_clone.id));
                                
                                Ok(())
                            }
                            Err(e) => {
                                error!("❌ Failed to fetch tasks for list {}: {}", list_clone.name, e);
                                Err(e)
                            }
                        }
                    });
                    
                    handles.push(handle);
                }
            }
            
            // Wait for all tasks to complete
            for handle in handles {
                if let Err(e) = handle.await {
                    error!("Task fetch handle error: {:?}", e);
                }
            }
            
            info!("🎯 All parallel task fetching completed");
            
            // Send a final signal to update all UI counts
            storage.send_cache_signal(CacheUpdateSignal::ListTasksUpdated("all".to_string()));
        });
        
        Ok(())
    }

    

    /// Helper method to fetch all pages of todo lists with pagination
    async fn fetch_all_todo_lists(&self, auth_token: &str) -> Result<Vec<TodoTaskList>, Error> {
        self.http_client
            .get_all_pages::<TodoTaskList, TodoTaskListCollection>(
                "/me/todo/lists",
                &format!("Bearer {}", auth_token),
            )
            .await
            .map_err(|_e| Error::Tasks(TasksError::ListNotFound))
    }

    /// Helper method to fetch all pages of todo tasks with pagination
    async fn fetch_all_todo_tasks(&self, url: &str, auth_token: &str) -> Result<Vec<TodoTask>, Error> {
        self.http_client
            .get_all_pages::<TodoTask, TodoTaskCollection>(
                url,
                &format!("Bearer {}", auth_token),
            )
            .await
            .map_err(|_e| {
                error!("❌ Failed to get tasks via API: {}   for url {}", _e, url);
                Error::Tasks(TasksError::ApiError)
            })
    }

    /// Helper method to fetch all pages of checklist items with pagination
    async fn fetch_all_checklist_items(&self, url: &str, auth_token: &str) -> Result<Vec<crate::integration::ms_todo::models::ChecklistItem>, Error> {
        self.http_client
            .get_all_pages::<crate::integration::ms_todo::models::ChecklistItem, crate::integration::ms_todo::models::ChecklistItemCollection>(
                url,
                &format!("Bearer {}", auth_token),
            )
            .await
            .map_err(|_e| {
                error!("❌ Failed to fetch checklist items via API: {}", _e);
                Error::Tasks(TasksError::ApiError)
            })
    }

    pub async fn lists(&mut self) -> Result<Vec<List>, Error> {
        let auth_token = self
            .get_valid_token()
            .map_err(|_e| Error::Tasks(TasksError::ListNotFound))?;

        // Fetch lists without task counts
        let response: TodoTaskListCollection = self
            .http_client
            .get(
                "/me/todo/lists",
                &format!("Bearer {}", auth_token),
            )
            .await
            .map_err(|_e| Error::Tasks(TasksError::ListNotFound))?;

        // Convert TodoTaskList[] → List[] (without task counts)
        let mut lists = Vec::new();
        for tl in response.value {
            let mut l: List = tl.into();
            l.number_of_tasks = 0; // Will be updated later via cache signals
            lists.push(l);
        }
        
        // Add virtual lists (also without task counts initially)
        let virtual_lists = vec![
            List::new_virtual(VirtualListType::MyDay, "My Day"),
            List::new_virtual(VirtualListType::Planned, "Planned"),
            List::new_virtual(VirtualListType::All, "All"),
        ];
        
        // Combine and sort: virtual lists first, then regular lists
        lists.extend(virtual_lists);
        lists.sort_by(|a, b| {
            if a.is_virtual && !b.is_virtual {
                std::cmp::Ordering::Less
            } else if !a.is_virtual && b.is_virtual {
                std::cmp::Ordering::Greater
            } else if a.is_virtual && b.is_virtual {
                a.sort_order.cmp(&b.sort_order)
            } else {
                a.well_known_list_name.cmp(&b.well_known_list_name).then(a.name.cmp(&b.name))
            }
        });
        
        // Spawn parallel task fetching AFTER returning lists
        let lists_for_fetching = lists.iter()
            .filter(|l| !l.is_virtual)
            .cloned()
            .collect();
        
        self.spawn_parallel_task_fetching(lists_for_fetching).await?;
        
        Ok(lists)
    }

    pub async fn create_task(&mut self, task: &Task) -> Result<Task, Error> {
        let list_id = task.list_id.clone().unwrap_or_default();
        let auth_token = self
            .get_valid_token()
            .map_err(|_e| Error::Tasks(TasksError::ExistingTask))?;

        info!("🔧 Creating task '{}' in list '{}'", task.title, list_id);

        info!("🔧 Task ID: {}", task.id);

        let request = CreateTodoTaskRequest::from(task);
        debug!(
            "Calling API to create task , {}",
            &format!("/me/todo/lists/{}/tasks", list_id)
        );
        let response: TodoTask = self
            .http_client
            .post(
                &format!("/me/todo/lists/{}/tasks", list_id),
                &request,
                &format!("Bearer {}", auth_token),
            )
            .await
            .map_err(|e| {
                error!("❌ Failed to create task via API: {}", e);
                Error::Tasks(TasksError::ApiError)
            })?;

        info!("✅ Task created successfully via API, ID: {}", response.id);

        // Convert TodoTask → Task with proper path construction
        let new_task =
            crate::integration::ms_todo::mapping::todo_task_to_task_with_path(response, &list_id);

        // Update cache with new task
        self.add_task_to_cache(new_task.clone()).await?;

        Ok(new_task)
    }

    pub async fn update_task(&mut self, task: &Task) -> Result<(), Error> {
        let list_id = task.list_id.clone().unwrap_or_default();
        let auth_token = self
            .get_valid_token()
            .map_err(|_e| Error::Tasks(TasksError::TaskNotFound))?;

        let request = UpdateTodoTaskRequest::from(task);
        info!("Request Json: {}", serde_json::to_string_pretty(&request).unwrap());
        info!("Request url: {}", &format!("/me/todo/lists/{}/tasks/{}", list_id, task.id));

        // PATCH returns the updated TodoTask
        let _: TodoTask = self
            .http_client
            .patch::<UpdateTodoTaskRequest, TodoTask>(
                &format!("/me/todo/lists/{}/tasks/{}", list_id, task.id),
                &request,
                &format!("Bearer {}", auth_token),
            )
            .await
            .map_err(|_e| Error::Tasks(TasksError::TaskNotFound))?;

        // Update cache with modified task
        self.update_task_in_cache(task.clone()).await?;

        Ok(())
    }

    pub async fn delete_task(&mut self, task: &Task) -> Result<(), Error> {
        let list_id = task.list_id.clone().unwrap_or_default();
        let task_id = task.id.clone();
        let auth_token = self
            .get_valid_token()
            .map_err(|_e| Error::Tasks(TasksError::TaskNotFound))?;

        self.http_client
            .delete(
                &format!("/me/todo/lists/{}/tasks/{}", list_id, task_id),
                &format!("Bearer {}", auth_token),
            )
            .await
            .map_err(|_e| Error::Tasks(TasksError::TaskNotFound))?;

        // Remove from cache
        self.remove_task_from_cache(&task_id, &list_id).await?;

        Ok(())
    }

    pub async fn create_list(&self, list: &List) -> Result<List, Error> {
        let auth_token = self
            .get_valid_token()
            .map_err(|_e| Error::Tasks(TasksError::ExistingList))?;

        let request = CreateTodoTaskListRequest::from(list);

        let response: TodoTaskList = self
            .http_client
            .post(
                "/me/todo/lists",
                &request,
                &format!("Bearer {}", auth_token),
            )
            .await
            .map_err(|_e| Error::Tasks(TasksError::ExistingList))?;

        // Convert TodoTaskList → List
        Ok(response.into())
    }

    pub async fn update_list(&self, list: &List) -> Result<(), Error> {
        let auth_token = self
            .get_valid_token()
            .map_err(|_e| Error::Tasks(TasksError::ListNotFound))?;

        let request = UpdateTodoTaskListRequest::from(list);

        
        let url = format!("/me/todo/lists/{}", list.id);
        
        // Use TodoTaskList as the response type since PATCH returns the updated list
        let _: TodoTaskList = self
            .http_client
            .patch::<UpdateTodoTaskListRequest, TodoTaskList>(
                &url, 
                &request,
                &format!("Bearer {}", auth_token),
            )
            .await.map_err(|e| {
                error!("❌ Failed to update list via API: {}", e);
                crate::app::error::Error::Tasks(TasksError::ApiError)}
            )?;

        Ok(())
    }

    pub async fn delete_list(&self, list: &List) -> Result<(), Error> {
        let auth_token = self
            .get_valid_token()
            .map_err(|_e| Error::Tasks(TasksError::ListNotFound))?;

        self.http_client
            .delete(
                &format!("/me/todo/lists/{}", list.id),
                &format!("Bearer {}", auth_token),
            )
            .await.map_err(|_e| crate::app::error::Error::Tasks(TasksError::ApiError))?;

        Ok(())
    }

    pub fn export_list(list: &List, tasks: &[Task]) -> String {
        let markdown = list.markdown();
        let tasks_markdown: String = tasks.iter().map(Markdown::markdown).collect();
        format!("{markdown}\n{tasks_markdown}")
    }

    // ============================================================================
    // Checklist Operations
    // ============================================================================

    /// Fetch checklist items for a task from MS Graph
    pub async fn fetch_checklist_items(&self, task: &Task) -> Result<Vec<crate::storage::models::ChecklistItem>, Error> {
        let auth_token = self
            .get_valid_token()
            .map_err(|_e| Error::Tasks(TasksError::TaskNotFound))?;

        // Extract list_id from task path or use stored list_id
        let list_id = task.list_id.as_ref()
            .ok_or_else(|| Error::Tasks(TasksError::TaskNotFound))?;

        let url = format!("/me/todo/lists/{}/tasks/{}/checklistItems", list_id, task.id);

        // Use helper method to fetch all pages with pagination
        let checklist_items = self.fetch_all_checklist_items(&url, &auth_token).await?;

        // Convert MS Graph ChecklistItem[] → local ChecklistItem[]
        let items: Vec<crate::storage::models::ChecklistItem> = checklist_items
            .into_iter()
            .map(|item| item.into())
            .collect();

        Ok(items)
    }

    /// Create a new checklist item via MS Graph
    pub async fn create_checklist_item(&self, task: &Task, title: &str) -> Result<crate::storage::models::ChecklistItem, Error> {
        let auth_token = self
            .get_valid_token()
            .map_err(|_e| Error::Tasks(TasksError::TaskNotFound))?;

        let list_id = task.list_id.as_ref()
            .ok_or_else(|| Error::Tasks(TasksError::ApiError))?;

        let request = crate::integration::ms_todo::models::CreateChecklistItemRequest {
            displayName: title.to_string(),
            isChecked: Some(false),
        };

        let url = format!("/me/todo/lists/{}/tasks/{}/checklistItems", list_id, task.id);

        let response: crate::integration::ms_todo::models::ChecklistItem = self
            .http_client
            .post(
                &url,
                &request,
                &format!("Bearer {}", auth_token),
            )
            .await
            .map_err(|_e| {
                error!("❌ Failed to create checklist item via API: {}", _e);
                Error::Tasks(TasksError::ApiError)
            })?;

        Ok(response.into())
    }

    /// Update a checklist item via MS Graph
    pub async fn update_checklist_item(&self, task: &Task, item_id: &str, title: &str, is_checked: bool) -> Result<crate::storage::models::ChecklistItem, Error> {
        let auth_token = self
            .get_valid_token()
            .map_err(|_e| Error::Tasks(TasksError::TaskNotFound))?;

        let list_id = task.list_id.as_ref()
            .ok_or_else(|| Error::Tasks(TasksError::ApiError))?;

        let request = crate::integration::ms_todo::models::UpdateChecklistItemRequest {
            displayName: Some(title.to_string()),
            isChecked: Some(is_checked),
        };

        let url = format!("/me/todo/lists/{}/tasks/{}/checklistItems/{}", list_id, task.id, item_id);

        let response: crate::integration::ms_todo::models::ChecklistItem = self
            .http_client
            .patch(
                &url,
                &request,
                &format!("Bearer {}", auth_token),
            )
            .await
            .map_err(|_e| {
                error!("❌ Failed to update checklist item via API: {}", _e);
                Error::Tasks(TasksError::ApiError)
            })?;

        Ok(response.into())
    }

    /// Delete a checklist item via MS Graph
    pub async fn delete_checklist_item(&self, task: &Task, item_id: &str) -> Result<String, Error> {
        let auth_token = self
            .get_valid_token()
            .map_err(|_e| Error::Tasks(TasksError::TaskNotFound))?;

        let list_id = task.list_id.as_ref()
            .ok_or_else(|| Error::Tasks(TasksError::ApiError))?;

        let url = format!("/me/todo/lists/{}/tasks/{}/checklistItems/{}", list_id, task.id, item_id);

        self.http_client
            .delete(
                &url,
                &format!("Bearer {}", auth_token),
            )
            .await
            .map_err(|_e| {
                error!("❌ Failed to delete checklist item via API: {}", _e);
                Error::Tasks(TasksError::ApiError)
            })?;

        Ok(item_id.to_string())
    }
}
