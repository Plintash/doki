ALTER TABLE `sessions` ADD `workspace` text DEFAULT '{"kind":"local"}' NOT NULL;
--> statement-breakpoint
UPDATE `sessions` SET `workspace` = COALESCE(
    (SELECT CASE WHEN json_valid(`data`) THEN json_extract(`data`, '$.workspace') END
     FROM `session_details` WHERE `session_id` = `sessions`.`id`),
    '{"kind":"local"}'
);
