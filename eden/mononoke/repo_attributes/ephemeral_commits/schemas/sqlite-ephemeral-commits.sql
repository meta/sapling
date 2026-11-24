/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

CREATE TABLE IF NOT EXISTS `ephemeral_commits` (
    `repo_id` INT NOT NULL,
    `bcs_id` BINARY(32) NOT NULL,
    `hg_cs_id` BINARY(20) NOT NULL,
    `created_at` BIGINT NOT NULL,
    `expires_at` BIGINT NOT NULL,
    PRIMARY KEY (`repo_id`, `bcs_id`),
    UNIQUE (`repo_id`, `hg_cs_id`)
);

CREATE INDEX IF NOT EXISTS `ephemeral_commits_repo_expires_at`
  ON `ephemeral_commits` (`repo_id`, `expires_at`);
