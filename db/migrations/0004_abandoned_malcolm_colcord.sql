ALTER TABLE `sessions` ADD `blocked_since` integer;--> statement-breakpoint
ALTER TABLE `sessions` ADD `blocked_reason` text;--> statement-breakpoint
ALTER TABLE `sessions` ADD `objective` text;--> statement-breakpoint
ALTER TABLE `sessions` ADD `turn_count` integer;--> statement-breakpoint
ALTER TABLE `sessions` ADD `changed_files` integer;--> statement-breakpoint
ALTER TABLE `sessions` ADD `archived_at` integer;