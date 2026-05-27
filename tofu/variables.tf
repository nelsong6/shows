variable "location" {
  description = "Azure region for the shows resource group"
  type        = string
  default     = "westus2"
}

# OIDC issuer URL of the AKS cluster that hosts the shows workloads.
# Stable per-cluster lifetime (changes only on cluster recreate). Lives
# in a separate subscription from this stack's default, so a tofu data
# source lookup would need a cross-subscription provider alias; using
# a variable keeps this stack provider-config-clean. Source:
#   az aks show -n infra-aks -g infra --subscription romaine-life \
#     --query oidcIssuerProfile.issuerUrl -o tsv
#
# Matches the value pinned in auth/tofu/variables.tf — same cluster.
variable "aks_oidc_issuer_url" {
  description = "OIDC issuer URL of the AKS cluster hosting shows workloads."
  type        = string
  default     = "https://westus2.oic.prod-aks.azure.com/2236b5e4-81d2-4d82-bde5-17b1037999ea/5aced6d5-4299-421b-84a9-6638aebbf4f0/"
}
