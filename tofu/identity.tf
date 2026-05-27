# Workload identity for the shows runtime pod. Replaces the
# (now-deleted) CNPG backup-writer UAMI — shows-api authenticates to
# the shared Cosmos account via DefaultAzureCredential picking up the
# projected SA token, exchanging it for a Cosmos data-plane token
# scoped to the shows database.
#
# Federated to the `shows` ServiceAccount in the `shows` namespace.
# Matches the chart's serviceaccount.yaml + deployment.yaml workload-
# identity annotations.
resource "azurerm_user_assigned_identity" "shows" {
  name                = "shows-identity"
  resource_group_name = azurerm_resource_group.shows.name
  location            = azurerm_resource_group.shows.location
}

resource "azurerm_federated_identity_credential" "shows" {
  name                = "aks-shows"
  resource_group_name = azurerm_resource_group.shows.name
  parent_id           = azurerm_user_assigned_identity.shows.id
  audience            = ["api://AzureADTokenExchange"]
  issuer              = local.aks_oidc_issuer_url
  subject             = "system:serviceaccount:shows:shows"
}
