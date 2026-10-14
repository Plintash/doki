ALTER TABLE `tasks` ADD `description` text DEFAULT '' NOT NULL;
--> statement-breakpoint
ALTER TABLE `sessions` ADD `is_draft` integer DEFAULT false NOT NULL;
