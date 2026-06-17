class AddGithubAppKindToBrokerCredentials < ActiveRecord::Migration[8.1]
  # github_app credentials mint GitHub App installation tokens (JWT -> POST
  # /app/installations/{id}/access_tokens) instead of running the OAuth
  # refresh_token grant. They reuse client_id for the app id (issuer) and need
  # an installation id plus the App private key (encrypted at rest). The default
  # kind keeps every existing row on the original refresh_token behavior.
  def change
    add_column :broker_credentials, :kind, :string, null: false, default: "oauth_refresh"
    add_column :broker_credentials, :installation_id, :string
    add_column :broker_credentials, :private_key, :text
  end
end
