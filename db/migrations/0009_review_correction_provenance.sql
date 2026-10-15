ALTER TABLE `sessions` ADD `review_correction_source_id` text;
--> statement-breakpoint
UPDATE `sessions` SET `review_correction_source_id` = (
    SELECT `source`.`id`
    FROM `sessions` AS `source`
    WHERE `source`.`provider` = 'codex'
      AND CASE WHEN json_valid(`source`.`review`)
          THEN json_extract(`source`.`review`, '$.fix_session_id') END = `sessions`.`id`
    ORDER BY `source`.`created_at`, `source`.`id`
    LIMIT 1
);
