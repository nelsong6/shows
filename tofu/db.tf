# Cosmos SQL database + containers for the shows playlist API.
#
# Three containers:
#   - playlists      one doc per playlist (currently just "nelson")
#   - shows          one doc per show, episodes embedded as an array
#   - watch_history  append-only log of played episodes
#
# Partition keys are sized for the single-user, ~40 show, ~1 write/day
# workload — single-partition queries dominate, cross-partition scans
# are rare and tolerable.

/*
resource "azurerm_cosmosdb_sql_database" "shows" {
  name                = "shows"
  resource_group_name = local.infra.resource_group_name
  account_name        = data.azurerm_cosmosdb_account.infra.name
  # No throughput specified — the account is serverless, so containers
  # below are also serverless and bill per-request.
  lifecycle {
    ignore_changes = [throughput]
  }
}

resource "azurerm_cosmosdb_sql_container" "playlists" {
  name                = "playlists"
  resource_group_name = local.infra.resource_group_name
  account_name        = data.azurerm_cosmosdb_account.infra.name
  database_name       = azurerm_cosmosdb_sql_database.shows.name
  partition_key_paths = ["/id"]

  indexing_policy {
    indexing_mode = "consistent"
    included_path { path = "/*" }
  }
}

# `shows` is the hot container. Partition by `/playlist` so all of a
# playlist's shows live in one partition — listing active shows for a
# playlist is a single-partition query. Each show doc embeds its
# episodes as an array; advance is a point read + point write on one
# show doc.
resource "azurerm_cosmosdb_sql_container" "shows" {
  name                = "shows"
  resource_group_name = local.infra.resource_group_name
  account_name        = data.azurerm_cosmosdb_account.infra.name
  database_name       = azurerm_cosmosdb_sql_database.shows.name
  partition_key_paths = ["/playlist"]

  indexing_policy {
    indexing_mode = "consistent"
    included_path { path = "/*" }
  }
}

# Append-only event log. Partition by /show_id so "history for show X"
# stays single-partition. Per-show histories rarely exceed a few hundred
# rows so partition size is bounded.
resource "azurerm_cosmosdb_sql_container" "watch_history" {
  name                = "watch_history"
  resource_group_name = local.infra.resource_group_name
  account_name        = data.azurerm_cosmosdb_account.infra.name
  database_name       = azurerm_cosmosdb_sql_database.shows.name
  partition_key_paths = ["/show_id"]

  indexing_policy {
    indexing_mode = "consistent"
    included_path { path = "/*" }
  }
}

# Cosmos DB Built-in Data Contributor (00000000-0000-0000-0000-000000000002)
# scoped to the shows database only — not the account. Other apps' data
# on the same infra-cosmos-serverless account stays unreachable from
# the shows pod even if compromised.
#
# `scope` uses the Cosmos data-plane path scheme (`/dbs/<name>`),
# distinct from the ARM resource ID (`/sqlDatabases/<name>`); passing
# the ARM ID gets rejected with "Expected path segment [dbs] at
# position [0] but found [sqlDatabases]." Pattern mirrors
# glimmung/tofu/identity.tf.
resource "azurerm_cosmosdb_sql_role_assignment" "shows_data_contributor" {
  resource_group_name = local.infra.resource_group_name
  account_name        = data.azurerm_cosmosdb_account.infra.name
  role_definition_id  = "${data.azurerm_cosmosdb_account.infra.id}/sqlRoleDefinitions/00000000-0000-0000-0000-000000000002"
  principal_id        = azurerm_user_assigned_identity.shows.principal_id
  scope               = "${data.azurerm_cosmosdb_account.infra.id}/dbs/${azurerm_cosmosdb_sql_database.shows.name}"
}
*/

