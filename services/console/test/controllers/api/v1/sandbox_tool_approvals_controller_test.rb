require "test_helper"

module Api
  module V1
    class SandboxToolApprovalsControllerTest < ActionDispatch::IntegrationTest
      test "only the verified current proxy supplies request identity" do
        proxy = proxies(:acme_proxy)
        captured = nil
        api = Object.new
        api.define_singleton_method(:request_tool_approval) do |payload|
          captured = payload
          { "id" => "00000000-0000-4000-8000-000000000001" }
        end
        with_env("CENTAUR_JWT_SIGNING_SECRET" => "test-secret") do
          CentaurApiClient.stub(:new, api) do
            post "/api/v1/sandbox/tool_approvals", as: :json,
              headers: { "Authorization" => "Bearer #{SandboxEntitlements::Jwt.encode_for_proxy(proxy)}" },
              params: { data: { action: "create", execution_id: "exe_test",
                idempotency_key: "00000000-0000-4000-8000-000000000002",
                principal_id: "prn_forged", sandbox_id: "forged", team_id: "TFORGED",
                arguments: { body: "frozen" } } }
          end
        end
        assert_response :accepted
        assert_equal proxy.name, captured[:sandbox_id]
        assert_equal proxy.principal.oid, captured[:principal_id]
        assert_not captured.key?("team_id")
        assert_equal({ "body" => "frozen" }, captured[:arguments])
        assert_equal "no-store", response.headers["Cache-Control"]
      end

      test "missing token and reassigned proxy cannot access approval endpoints" do
        get "/api/v1/sandbox/tool_approvals/context"
        assert_response :unauthorized
        with_env("CENTAUR_JWT_SIGNING_SECRET" => "test-secret") do
          proxy = proxies(:acme_proxy)
          token = SandboxEntitlements::Jwt.encode_for_proxy(proxy)
          proxy.update!(principal: principals(:globex_user))
          get "/api/v1/sandbox/tool_approvals/context", headers: { "Authorization" => "Bearer #{token}" }
        end
        assert_response :unauthorized
      end
    end
  end
end
