output "resource_group_name" {
  value       = azurerm_resource_group.shows.name
  description = "Name of the shows resource group"
}

output "shows_db_backups_storage_account" {
  value       = azurerm_storage_account.shows_db_backups.name
  description = "Storage account holding shows-db CNPG backups. Drives the destinationPath in k8s/templates/cluster.yaml — copy to k8s/values.yaml::backups.storageAccount after the first apply."
}

output "shows_db_backups_container" {
  value       = azurerm_storage_container.shows_db_backups.name
  description = "Container within the backups storage account. Fixed at 'shows-db'."
}

output "shows_db_backup_writer_client_id" {
  value       = azurerm_user_assigned_identity.shows_db_backup_writer.client_id
  description = "Client ID of the UAMI federated to the shows-db ServiceAccount. Copy into k8s/values.yaml::backups.workloadIdentityClientId after the first apply."
}
