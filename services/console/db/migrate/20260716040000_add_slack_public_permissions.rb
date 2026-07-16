class AddSlackPublicPermissions < ActiveRecord::Migration[8.1]
  def change
    change_table :principals, bulk: true do |t|
      t.boolean :slack_public_history_enabled, default: false, null: false
      t.boolean :slack_public_download_enabled, default: false, null: false
      t.boolean :slack_public_upload_enabled, default: false, null: false
    end

    change_table :system_settings, bulk: true do |t|
      t.boolean :default_slack_public_history_enabled, default: false, null: false
      t.boolean :default_slack_public_download_enabled, default: false, null: false
      t.boolean :default_slack_public_upload_enabled, default: false, null: false
    end
  end
end
