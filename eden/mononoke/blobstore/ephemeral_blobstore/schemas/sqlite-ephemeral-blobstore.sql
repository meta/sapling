/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

CREATE TABLE IF NOT EXISTS `ephemeral_bubbles` (
  `id` INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
  `created_at` BIGINT NOT NULL,
  `expires_at` BIGINT NOT NULL,
  `expired` BIT NOT NULL DEFAULT '0',
  `owner_identity` VARCHAR(255)
);

CREATE INDEX IF NOT EXISTS `ephemeral_bubbles_expires`
  ON `ephemeral_bubbles` (`expires_at`, `id`);

CREATE TABLE IF NOT EXISTS `ephemeral_bubble_changeset_mapping` (
    `repo_id` INT NOT NULL,
    `cs_id` BINARY(32) NOT NULL,
    `bubble_id` BIGINT UNSIGNED NOT NULL,
    `gen` BIGINT NOT NULL,
    PRIMARY KEY (`repo_id`, `bubble_id`, `cs_id`)
);

CREATE TABLE IF NOT EXISTS `ephemeral_bubble_changeset_parents` (
    `repo_id` INT NOT NULL,
    `bubble_id` BIGINT UNSIGNED NOT NULL,
    `cs_id` BINARY(32) NOT NULL,
    `parent_index` INT NOT NULL,
    `parent_cs_id` BINARY(32) NOT NULL,
    PRIMARY KEY (`repo_id`, `bubble_id`, `cs_id`, `parent_index`)
);

CREATE TABLE IF NOT EXISTS 'ephemeral_bubble_labels' (
    `bubble_id` BIGINT UNSIGNED NOT NULL,
    `label` VARCHAR(255),
    PRIMARY KEY (`bubble_id`, `label`)
);

CREATE TABLE IF NOT EXISTS `ephemeral_bubble_hg_mapping` (
    `repo_id` INT NOT NULL,
    `hg_cs_id` BINARY(20) NOT NULL,
    `bubble_id` BIGINT UNSIGNED NOT NULL,
    `bcs_id` BINARY(32) NOT NULL,
    PRIMARY KEY (`repo_id`, `hg_cs_id`),
    UNIQUE (`repo_id`, `bcs_id`)
);

CREATE INDEX IF NOT EXISTS `ephemeral_bubble_hg_mapping_bubble_id`
  ON `ephemeral_bubble_hg_mapping` (`bubble_id`);
