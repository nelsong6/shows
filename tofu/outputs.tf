output "resource_group_name" {
  value       = azurerm_resource_group.shows.name
  description = "Name of the shows resource group"
}

output "shows_identity_client_id" {
  value       = azurerm_user_assigned_identity.shows.client_id
  description = "Client ID of the shows-identity UAMI federated to system:serviceaccount:shows:shows. Pin into k8s/values.yaml::identity.workloadIdentityClientId."
}

output "cosmos_endpoint" {
  value       = data.azurerm_cosmosdb_account.infra.endpoint
  description = "Cosmos account endpoint URL (https://<account>.documents.azure.com:443/). Pin into k8s/values.yaml::cosmos.endpoint."
}

output "cosmos_database_name" {
  value       = azurerm_cosmosdb_sql_database.shows.name
  description = "Name of the shows Cosmos database. Pin into k8s/values.yaml::cosmos.database."
}
