# References to shared infrastructure provisioned by infra-bootstrap.
# Names are stored here; IDs + cross-stack outputs are resolved via data
# sources / remote state at plan time so this stack doesn't have to
# duplicate values.

locals {
  infra = {
    resource_group_name = "infra"
    key_vault_name      = "romaine-kv"
  }
}

data "azuread_client_config" "current" {}
data "azurerm_client_config" "current" {}

# Shared serverless Cosmos account that backs every per-app database.
# Pattern matches glimmung/tofu/remote-state.tf — Cosmos data-plane RBAC
# is granted per-database below in db.tf, not at account scope.
data "azurerm_cosmosdb_account" "infra" {
  name                = "infra-cosmos-serverless"
  resource_group_name = local.infra.resource_group_name
}

# AKS OIDC issuer URL pulled from infra-bootstrap's tofu state instead
# of hardcoded. Survives cluster recreate without a per-app variable
# bump. The infra-bootstrap app module already grants this CI principal
# Storage Blob Data Contributor (via runs_own_tofu_apps), which covers
# reading the state container.
data "terraform_remote_state" "infra_bootstrap" {
  backend = "azurerm"

  config = {
    resource_group_name  = "infra"
    storage_account_name = "nelsontofu"
    container_name       = "tfstate"
    key                  = "infra-bootstrap.tfstate"
    use_oidc             = true
  }
}

locals {
  aks_oidc_issuer_url = data.terraform_remote_state.infra_bootstrap.outputs.aks_oidc_issuer_url
}

# romaine-kv data source — no consumers today, kept for parity with
# auth/glimmung so adding a KV-backed secret later is a one-line change.
data "azurerm_key_vault" "main" {
  name                = local.infra.key_vault_name
  resource_group_name = local.infra.resource_group_name
}
