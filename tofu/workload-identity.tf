# Workload-identity infrastructure for the shows-db CNPG cluster's
# backup writes. Mirrors auth/tofu/workload-identity.tf: a dedicated UAMI
# federated to the cluster's ServiceAccount via the AKS OIDC issuer, with
# Storage Blob Data Contributor scoped to one container.

# Dedicated UAMI for CNPG backup writes. Scope is minimal: Storage Blob
# Data Contributor on the single container that holds shows-db backups.
resource "azurerm_user_assigned_identity" "shows_db_backup_writer" {
  name                = "shows-db-backup-writer"
  resource_group_name = azurerm_resource_group.shows.name
  location            = azurerm_resource_group.shows.location
}

# Federated credential trusts tokens issued by the AKS OIDC issuer for
# the shows-db ServiceAccount. CNPG creates the SA named after the
# Cluster ("shows-db"); confirm with `kubectl get sa -n shows`.
resource "azurerm_federated_identity_credential" "shows_db_backup_writer" {
  name                = "shows-db"
  resource_group_name = azurerm_resource_group.shows.name
  parent_id           = azurerm_user_assigned_identity.shows_db_backup_writer.id
  audience            = ["api://AzureADTokenExchange"]
  issuer              = var.aks_oidc_issuer_url
  subject             = "system:serviceaccount:shows:shows-db"
}

# Storage Blob Data Contributor at container scope (NOT account scope)
# so a compromised pod can't reach into other workloads' blobs in the
# same account.
resource "azurerm_role_assignment" "shows_db_backup_writer_blob_contributor" {
  scope                = azurerm_storage_container.shows_db_backups.resource_manager_id
  role_definition_name = "Storage Blob Data Contributor"
  principal_id         = azurerm_user_assigned_identity.shows_db_backup_writer.principal_id
}
