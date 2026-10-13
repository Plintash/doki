CREATE TABLE `tasks` (
	`id` text PRIMARY KEY NOT NULL,
	`title` text NOT NULL,
	`state` text NOT NULL,
	`position` integer NOT NULL,
	`project_id` text,
	`path` text,
	`branch` text,
	`worktree` integer DEFAULT false NOT NULL,
	`created_at` integer NOT NULL,
	`updated_at` integer NOT NULL,
	`completed_at` integer
);
--> statement-breakpoint
CREATE INDEX `tasks_by_state` ON `tasks` (`state`,`position`);--> statement-breakpoint
ALTER TABLE `sessions` ADD `task_id` text;--> statement-breakpoint
CREATE INDEX `sessions_by_task` ON `sessions` (`task_id`);