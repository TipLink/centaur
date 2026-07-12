# frozen_string_literal: true

module SecurityCredentialValidation
  MINIMUM_BYTES = 32
  CREDENTIAL_ENV_LANES = [
    %w[CENTAUR_CONSOLE_CENTAUR_API_KEY IRON_CONTROL_CENTAUR_API_KEY],
    %w[SLACKBOT_API_KEY],
    %w[GITHUBBOT_API_KEY],
    %w[LINEARBOT_API_KEY],
    %w[DISCORDBOT_API_KEY],
    %w[TEAMSBOT_API_KEY],
    %w[WORKFLOW_API_KEY],
    %w[SLACK_FEEDBACK_API_KEY],
    %w[CENTAUR_JWT_SIGNING_SECRET]
  ].freeze

  def self.validate!(environment = ENV)
    owners = {}

    CREDENTIAL_ENV_LANES.each do |env_names|
      env_name = env_names.first
      value = configured_value(environment, env_names).to_s.strip
      next if value.empty?

      if value.bytesize < MINIMUM_BYTES
        raise ArgumentError, "#{env_name} must contain at least #{MINIMUM_BYTES} bytes"
      end

      existing = owners[value]
      if existing
        raise ArgumentError, "#{existing} and #{env_name} must contain distinct service credentials"
      end

      owners[value] = env_name
    end

    true
  end

  def self.configured_value(environment, env_names)
    env_names.each do |env_name|
      return environment[env_name] if environment.key?(env_name)
    end
    nil
  end
  private_class_method :configured_value
end
