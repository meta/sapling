/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

CREATE TABLE IF NOT EXISTS `bookmarks` (
  `repo_id` INT UNSIGNED NOT NULL,
  `name` VARCHAR(512) NOT NULL,
  `changeset_id` VARBINARY(32) NOT NULL,
  -- this column is named 'hg_kind' for historical reasons, but applies for non-Mercurial uses (e.g. phase calculations)
  `hg_kind` VARCHAR(32) NOT NULL DEFAULT (CAST('pull_default' AS BLOB)), -- enum is used in mysql
  -- Distinguish between different categories of bookmarks:
  -- branch, tag or note
  `category` VARCHAR(32) NOT NULL DEFAULT (CAST('branch' AS BLOB)),
  `log_id` INTEGER NULL,
  PRIMARY KEY (`repo_id`, `name`, `category`)
);

CREATE INDEX IF NOT EXISTS `bookmarks_repo_id_log_id` ON `bookmarks` (`repo_id`, `log_id`);
CREATE INDEX IF NOT EXISTS `bookmarks_repo_id_hg_kind` ON `bookmarks` (`repo_id`, `hg_kind`);

CREATE TABLE IF NOT EXISTS `bookmarks_update_log` (
  `id` INTEGER NOT NULL,
  `repo_id` INT UNSIGNED NOT NULL,
  `name` VARCHAR(512) NOT NULL,
  `from_changeset_id` VARBINARY(32),
  `to_changeset_id` VARBINARY(32),
  `reason` VARCHAR(32) NOT NULL, -- enum is used in mysql
  `timestamp` BIGINT NOT NULL,
  `category` VARCHAR(32) NOT NULL DEFAULT (CAST('branch' AS BLOB)),
  PRIMARY KEY (`repo_id`, `name`, `id`)
);

CREATE INDEX IF NOT EXISTS `bookmarks_update_log_repo_id_id` ON `bookmarks_update_log` (`repo_id`, `id`);

CREATE TABLE IF NOT EXISTS `bookmarks_update_log_lock` (
  `repo_id` INT UNSIGNED NOT NULL,
  `id` INTEGER NOT NULL,
  PRIMARY KEY (`repo_id`, `id`)
);
