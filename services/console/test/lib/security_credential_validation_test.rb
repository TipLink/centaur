# frozen_string_literal: true

require "test_helper"

class SecurityCredentialValidationTest < ActiveSupport::TestCase
  test "rejects short signing keys without exposing the value" do
    error = assert_raises(ArgumentError) do
      SecurityCredentialValidation.validate!("CENTAUR_JWT_SIGNING_SECRET" => "short")
    end

    assert_includes error.message, "CENTAUR_JWT_SIGNING_SECRET must contain at least 32 bytes"
    refute_includes error.message, "short"
  end

  test "rejects a signing key reused as the control credential" do
    shared = "shared-key-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
    error = assert_raises(ArgumentError) do
      SecurityCredentialValidation.validate!(
        "CENTAUR_JWT_SIGNING_SECRET" => shared,
        "CENTAUR_CONSOLE_CENTAUR_API_KEY" => shared
      )
    end

    assert_includes error.message, "must contain distinct service credentials"
    refute_includes error.message, shared
  end

  test "rejects a signing key reused through the legacy control credential name" do
    shared = "shared-key-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
    error = assert_raises(ArgumentError) do
      SecurityCredentialValidation.validate!(
        "CENTAUR_JWT_SIGNING_SECRET" => shared,
        "IRON_CONTROL_CENTAUR_API_KEY" => shared
      )
    end

    assert_includes error.message, "CENTAUR_CONSOLE_CENTAUR_API_KEY"
    assert_includes error.message, "must contain distinct service credentials"
    refute_includes error.message, shared
  end

  test "an explicitly empty canonical control credential suppresses the legacy fallback" do
    assert SecurityCredentialValidation.validate!(
      "CENTAUR_CONSOLE_CENTAUR_API_KEY" => "",
      "IRON_CONTROL_CENTAUR_API_KEY" => "short",
      "CENTAUR_JWT_SIGNING_SECRET" => "jwt-signing-key-xxxxxxxxxxxxxxxxxxxxxxxxxxx"
    )
  end

  test "a configured canonical control credential wins over the legacy fallback" do
    jwt = "jwt-signing-key-xxxxxxxxxxxxxxxxxxxxxxxxxxx"
    assert SecurityCredentialValidation.validate!(
      "CENTAUR_CONSOLE_CENTAUR_API_KEY" => "control-key-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
      "IRON_CONTROL_CENTAUR_API_KEY" => jwt,
      "CENTAUR_JWT_SIGNING_SECRET" => jwt
    )
  end

  test "accepts distinct sufficiently long credentials" do
    assert SecurityCredentialValidation.validate!(
      "CENTAUR_JWT_SIGNING_SECRET" => "jwt-signing-key-xxxxxxxxxxxxxxxxxxxxxxxxxxx",
      "CENTAUR_CONSOLE_CENTAUR_API_KEY" => "control-key-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
    )
  end
end
