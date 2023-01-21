// Copyright 2023 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// TODO: Finish tests for `prev` and `next`

use crate::common::{get_stderr_string, TestEnvironment};

pub mod common;

#[test]
fn test_next_simple() {
    let test_env = TestEnvironment::default();
    test_env.jj_cmd_success(test_env.env_root(), &["init", "repo", "--git"]);
    let repo_path = test_env.env_root().join("repo");
    // Create a simple linear history, which we'll traverse.
    test_env.jj_cmd_success(test_env.env_root(), &["commit", "-m", "first"]);
    test_env.jj_cmd_success(test_env.env_root(), &["commit", "-m", "second"]);
    test_env.jj_cmd_success(test_env.env_root(), &["commit", "-m", "third"]);
    test_env.jj_cmd_success(test_env.env_root(), &["edit", ""]);
    test_env.jj_cmd_success(test_env.env_root(), &["next"]);
    insta::assert_snapshot!()
}

#[test]
fn test_next_multiple_without_root() {
    let test_env = TestEnvironment::default();
    test_env.jj_cmd_success(test_env.env_root(), &["init", "repo", "--git"]);
    let repo_path = test_env.env_root().join("repo");
    insta::assert_snapshot!()
}

#[test]
fn test_prev_simple() {
    let test_env = TestEnvironment::default();
    test_env.jj_cmd_success(test_env.env_root(), &["init", "repo", "--git"]);
    let repo_path = test_env.env_root().join("repo");
    test_env.jj_cmd_success(test_env.env_root(), &["commit", "-m", "first"]);
    test_env.jj_cmd_success(test_env.env_root(), &["commit", "-m", "second"]);
    test_env.jj_cmd_success(test_env.env_root(), &["commit", "-m", "third"]);
    test_env.jj_cmd_success(test_env.env_root(), &["prev"]);
    // The working copy commit is now a child of "second".
    insta::assert_snapshot!()
}

#[test]
fn test_prev_multiple_without_root() {
    let test_env = TestEnvironment::default();
    test_env.jj_cmd_success(test_env.env_root(), &["init", "repo", "--git"]);
    let repo_path = test_env.env_root().join("repo");
}

#[test]
fn test_next_fails_on_branching_children() {
    // TODO(#NNN): Fix this behavior
    let test_env = TestEnvironment::default();
    test_env.jj_cmd_success(test_env.env_root(), &["init", "repo", "--git"]);
    let repo_path = test_env.env_root().join("repo");
}

#[test]
fn test_prev_fails_on_multiple_parents() {
    // TODO(#NNN): Fix this behavior
    let test_env = TestEnvironment::default();
    test_env.jj_cmd_success(test_env.env_root(), &["init", "repo", "--git"]);
    let repo_path = test_env.env_root().join("repo");
}

#[test]
fn test_prev_onto_root_fails() {
    let test_env = TestEnvironment::default();
    test_env.jj_cmd_success(test_env.env_root(), &["init", "repo", "--git"]);
    let repo_path = test_env.env_root().join("repo");
}

#[test]
fn test_prev_editing() {
    let test_env = TestEnvironment::default();
    test_env.jj_cmd_success(test_env.env_root(), &["init", "repo", "--git"]);
    let repo_path = test_env.env_root().join("repo");
}

#[test]
fn test_next_editing() {
    let test_env = TestEnvironment::default();
    test_env.jj_cmd_success(test_env.env_root(), &["init", "repo", "--git"]);
    let repo_path = test_env.env_root().join("repo");
}
