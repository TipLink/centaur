module Api
  module V1
    module Sandbox
      # This boundary attests identity only. api-rs owns approval state and policy.
      class ToolApprovalsController < Api::SandboxBaseController
        before_action :disable_caching
        rescue_from CentaurApiClient::Error, with: :render_api_error

        def context
          render json: { data: api.tool_approval_context(identity) }
        end

        def create
          body = params.require(:data)
          unless body.is_a?(ActionController::Parameters)
            return render_error(status: :bad_request, message: "data must be a JSON object")
          end
          attributes = body.permit(:execution_id, :idempotency_key, :action).to_h
          arguments = body[:arguments]
          unless arguments.is_a?(ActionController::Parameters)
            return render_error(status: :bad_request, message: "arguments must be a JSON object")
          end
          # Arguments are opaque tool input. Identity and destination are never
          # taken from the request, including nested fields with those names.
          attributes[:arguments] = arguments.to_unsafe_h
          render json: { data: api.request_tool_approval(attributes.merge(identity)) }, status: :accepted
        end

        def show
          unless /\A[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\z/.match?(params[:id].to_s)
            return render_error(status: :not_found, message: "tool approval not found")
          end
          render json: { data: api.read_tool_approval(params[:id], identity) }
        end

        def cancel
          unless /\A[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\z/.match?(params[:id].to_s)
            return render_error(status: :not_found, message: "tool approval not found")
          end
          render json: { data: api.cancel_tool_approval(params[:id], identity) }
        end

        private

        def identity
          { sandbox_id: current_proxy.name, principal_id: current_proxy.principal.oid }
        end

        def api
          @api ||= CentaurApiClient.new(timeout: 2)
        end

        def disable_caching
          response.headers["Cache-Control"] = "no-store"
        end

        def render_api_error(error)
          status = [ 400, 403, 404, 413 ].include?(error.status) ? error.status : 503
          render_error(status: status, message: "tool approval request unavailable")
        end
      end
    end
  end
end
