# References to shared infrastructure provisioned by infra-bootstrap.
# Only names are stored here; full IDs are resolved via data source
# lookups at plan time so this stack doesn't have to import remote state.

locals {
  infra = {
    resource_group_name = "infra"
    key_vault_name      = "romaine-kv"
  }
}

# romaine-kv is referenced here for parity with the auth tofu stack —
# adding KV-backed secrets (e.g. a future webhook signing secret) lands
# in this stack and uses this data source. No consumers today; keep the
# lookup so adding one later is a one-line change.
data "azurerm_key_vault" "main" {
  name                = local.infra.key_vault_name
  resource_group_name = local.infra.resource_group_name
}
