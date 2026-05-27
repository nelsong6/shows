# Backup destination for the shows-db CNPG Cluster. CloudNativePG's
# barmanObjectStore handles continuous WAL archiving and periodic base
# backups; the destination here is a dedicated Azure Storage Account
# scoped to one container so a compromised credential can't reach any
# other workload's data.
#
# Auth model: workload identity. CNPG pods authenticate to Storage via
# the `shows-db-backup-writer` UAMI (provisioned in workload-identity.tf)
# bound to the shows-db ServiceAccount via federated credential. The
# UAMI holds Storage Blob Data Contributor at CONTAINER scope only.

# Storage account names must be globally unique, lowercase alphanumeric,
# 3-24 chars. random_string suffix keeps it deterministic per state file
# while avoiding collisions across environments.
resource "random_string" "backups_suffix" {
  length  = 6
  upper   = false
  special = false
  numeric = true
}

resource "azurerm_storage_account" "shows_db_backups" {
  name                = "showsdbbackups${random_string.backups_suffix.result}"
  resource_group_name = azurerm_resource_group.shows.name
  location            = azurerm_resource_group.shows.location

  account_tier             = "Standard"
  account_replication_type = "LRS"
  account_kind             = "StorageV2"

  # Storage account-level public access stays enabled; the container
  # below is configured private so blobs require auth.
  public_network_access_enabled   = true
  allow_nested_items_to_be_public = false

  min_tls_version = "TLS1_2"

  # Soft-delete keeps deleted blobs/containers around for 30 days so an
  # accidental delete (CNPG bug, mis-typed retention policy) is
  # recoverable. Cheap insurance — costs storage for the soft-deleted
  # period only.
  blob_properties {
    delete_retention_policy {
      days = 30
    }
    container_delete_retention_policy {
      days = 30
    }
  }
}

resource "azurerm_storage_container" "shows_db_backups" {
  name                  = "shows-db"
  storage_account_id    = azurerm_storage_account.shows_db_backups.id
  container_access_type = "private"
}
