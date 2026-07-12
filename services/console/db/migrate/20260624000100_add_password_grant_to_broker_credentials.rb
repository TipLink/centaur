class AddPasswordGrantToBrokerCredentials < ActiveRecord::Migration[8.1]
  def up
    add_column :broker_credentials, :grant, :string, default: "refresh_token" unless column_exists?(:broker_credentials, :grant)

    if column_exists?(:broker_credentials, :credential_kind)
      execute <<~SQL.squish
        UPDATE broker_credentials
        SET "grant" = CASE credential_kind
          WHEN 'oauth_refresh_token' THEN 'refresh_token'
          ELSE credential_kind
        END
        WHERE credential_kind IS NOT NULL
      SQL
      remove_column :broker_credentials, :credential_kind
    end

    change_column_null :broker_credentials, :grant, false
    add_column :broker_credentials, :username, :text unless column_exists?(:broker_credentials, :username)
    add_column :broker_credentials, :password, :text unless column_exists?(:broker_credentials, :password)
  end

  def down
    add_column :broker_credentials, :credential_kind, :string, default: "oauth_refresh_token" unless column_exists?(:broker_credentials, :credential_kind)
    if column_exists?(:broker_credentials, :grant)
      execute <<~SQL.squish
        UPDATE broker_credentials
        SET credential_kind = CASE "grant"
          WHEN 'refresh_token' THEN 'oauth_refresh_token'
          ELSE "grant"
        END
        WHERE "grant" IS NOT NULL
      SQL
    end
    change_column_null :broker_credentials, :credential_kind, false

    remove_column :broker_credentials, :password if column_exists?(:broker_credentials, :password)
    remove_column :broker_credentials, :username if column_exists?(:broker_credentials, :username)
    remove_column :broker_credentials, :grant if column_exists?(:broker_credentials, :grant)
  end
end
