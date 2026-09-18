require "test_helper"
require "erb"
require "yaml"

class DatabaseConfigurationTest < ActiveSupport::TestCase
  test "production databases use the configured Console database name" do
    with_env("CENTAUR_CONSOLE_DATABASE_NAME" => "iron_control_shadow") do
      production = rendered_database_config.fetch("production")

      assert_equal "iron_control_shadow", production.dig("primary", "database")
      assert_equal "iron_control_shadow_cache", production.dig("cache", "database")
      assert_equal "iron_control_shadow_queue", production.dig("queue", "database")
      assert_equal "iron_control_shadow_cable", production.dig("cable", "database")
    end
  end

  private

  def rendered_database_config
    template = ERB.new(Rails.root.join("config/database.yml").read).result
    YAML.safe_load(template, aliases: true)
  end

  def with_env(values)
    original = values.to_h { |key, _value| [ key, ENV[key] ] }
    values.each { |key, value| ENV[key] = value }
    yield
  ensure
    original.each { |key, value| ENV[key] = value }
  end
end
