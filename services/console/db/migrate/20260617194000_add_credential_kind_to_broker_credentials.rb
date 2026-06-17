class AddCredentialKindToBrokerCredentials < ActiveRecord::Migration[8.1]
  def change
    add_column :broker_credentials, :credential_kind, :string,
               null: false, default: "oauth_refresh_token"
  end
end
