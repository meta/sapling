# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

### GraphQL queries

GRAPHQL_GET_REPOSITORY = """
query ($owner: String!, $name: String!) {
  repository(name: $name, owner: $owner) {
    id
    owner {
      id
      login
    }
    name
    isFork
    defaultBranchRef {
      name
    }
    parent {
      id
      owner {
        id
        login
      }
      name
      isFork
      defaultBranchRef {
        name
      }
    }
  }
}
"""

GRAPHQL_GET_PULL_REQUEST = """
query ($owner: String!, $name: String!, $number: Int!) {
  repository(name: $name, owner: $owner) {
    pullRequest(number: $number) {
      id
      url
      state
      baseRefOid
      baseRefName
      headRefOid
      headRefName
      body
      title
    }
  }
}
"""

GRAPHQL_GET_MAX_PR_ISSUE_NUMBER = """
query ($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) {
    issues(orderBy: {field: CREATED_AT, direction: ASC}, last: 1) {
      nodes {
        number
      }
    }
    pullRequests(orderBy: {field: CREATED_AT, direction: ASC}, last: 1) {
      nodes {
        number
      }
    }
  }
}
"""

GRAPHQL_UPDATE_PULL_REQUEST = """
mutation ($pullRequestId: ID!, $title: String!, $body: String!, $base: String!) {
  updatePullRequest(
    input: {pullRequestId: $pullRequestId, title: $title, body: $body, baseRefName: $base}
  ) {
    pullRequest {
      id
    }
  }
}
"""

# Like GRAPHQL_UPDATE_PULL_REQUEST, but does not touch the base branch.
# GitHub rejects updatePullRequest mutations that include baseRefName for
# pull requests that are part of a native stack (the stack manages base
# branches itself), so this variant is used to update only the title/body.
GRAPHQL_UPDATE_PULL_REQUEST_NO_BASE = """
mutation ($pullRequestId: ID!, $title: String!, $body: String!) {
  updatePullRequest(
    input: {pullRequestId: $pullRequestId, title: $title, body: $body}
  ) {
    pullRequest {
      id
    }
  }
}
"""

GRAPHQL_CREATE_BRANCH = """
mutation ($repositoryId: ID!, $name: String!, $oid: GitObjectID!) {
  createRef(input: {repositoryId: $repositoryId, name: $name, oid: $oid}) {
    ref {
      id
    }
  }
}
"""

GRAPHQL_MERGE_BRANCH = """
mutation ($repositoryId: ID!, $base: String!, $head: String!) {
  mergeBranch(input: {repositoryId: $repositoryId, base: $base, head: $head}) {
    mergeCommit {
      oid
    }
  }
}
"""

GRAPHQL_GET_LOGIN = """
query {
  viewer {
    login
  }
}
"""

GRAPHQL_PR_TO_REF = """
query ($owner: String!, $name: String!, $number: Int!) {
    repository(name: $name, owner: $owner) {
        pullRequest(number: $number) {
            headRefName
        }
    }
}
"""

GRAPHQL_REF_TO_COMMIT_AND_TREE = """
query ($repo_id: ID!, $ref: String!) {
  node(id: $repo_id) {
    ... on Repository {
      ref(qualifiedName: $ref) {
        target {
          oid
          ... on Commit {
            tree {
              oid
            }
          }
        }
      }
    }
  }
}
"""
