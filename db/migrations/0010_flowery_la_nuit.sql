ALTER TABLE `projects` ADD `board` text DEFAULT '{"revision":0,"stages":[]}' NOT NULL;--> statement-breakpoint
ALTER TABLE `tasks` ADD `stage_id` text;