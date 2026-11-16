// The durable request carries a validated catalogue. User-facing text is always plain text.
import type { JsonObject } from './types'
import { isJsonObject } from './utils'

export type TicketForm = {
  title: string
  summary: string
  team: string
  project: string | null
  due: string | null
  teams: Record<string, { id: string; name: string }[]>
}
export type TicketSelection = Pick<TicketForm, 'team' | 'project' | 'due'> & { context: string }

export const TEAM_ACTION = 'centaur.user_input:team:'
const plain = (text: string) => ({ type: 'plain_text', text })
const option = (value: string, label: string) => ({ value, text: plain([...label].slice(0, 75).join('')) })

function input(block_id: string, label: string, element: JsonObject, optional = false) {
  return { type: 'input', block_id, label: plain(label), optional, element }
}

function select(action_id: string, choices: ReturnType<typeof option>[], value: string) {
  const initial_option = choices.find(choice => choice.value === value)
  if (choices.length <= 100) return { type: 'static_select', action_id, initial_option, options: choices }
  const option_groups = []
  for (let start = 0; start < choices.length; start += 100) {
    const options = choices.slice(start, start + 100)
    option_groups.push({ label: plain(`Projects ${start + 1}–${start + options.length}`), options })
  }
  return { type: 'static_select', action_id, initial_option, option_groups }
}

export function initialTicketSelection(form: TicketForm): TicketSelection {
  return { team: form.team, project: form.project, due: form.due, context: '' }
}

export function ticketMessage(form: TicketForm, draft: TicketSelection, id: string, marker: string, submit: JsonObject, notice?: string) {
  const title = `Create Ticket Form: ${form.title}`
  const teams = Object.keys(form.teams).sort().map(team => option(team, team))
  const projects = form.teams[draft.team]!.map(p => option(p.id, p.name))
  projects.push(option('none', 'No project'))
  const team = input(`team.${draft.team}`, 'Team', select(TEAM_ACTION + id, teams, draft.team))
  const children = [
    { type: 'section', text: plain(form.summary) },
    ...(notice ? [{ type: 'context', elements: [plain(notice)] }] : []),
    { ...team, dispatch_action: true },
    input(`project.${draft.team}`, 'Project', select('project', projects, draft.project ?? 'none')),
    input('due', 'Due date', { type: 'datepicker', action_id: 'date', placeholder: plain('Choose a date'),
      ...(draft.due ? { initial_date: draft.due } : {}) }, true),
    input('context', 'Additional context', { type: 'plain_text_input', action_id: 'note', multiline: true,
      placeholder: plain("Add anything the thread hasn’t covered."), max_length: 2000, ...(draft.context ? { initial_value: draft.context } : {}) }, true),
    submit
  ]
  return { text: [title, form.summary, draft.team, form.teams[draft.team]!.find(p => p.id === draft.project)?.name ?? 'No project',
    draft.due, notice].filter(Boolean).join('\n'), mrkdwn: false, unfurl_links: false, unfurl_media: false,
    blocks: [{ type: 'container', block_id: marker, title: plain(title), has_header_divider: true, child_blocks: children }] }
}

function actionValue(values: JsonObject, block: string, action: string): JsonObject {
  const group = values[block]
  if (!isJsonObject(group)) return {}
  const element = group[action]
  return isJsonObject(element) ? element : {}
}

function selected(action: JsonObject): unknown {
  return isJsonObject(action.selected_option) ? action.selected_option.value : undefined
}

// Read the project from the menu for the *selected* team. A click on Submit can race
// the team-change webhook, so a stale menu can never authorize a different team's project.
export function readTicketSelection(form: TicketForm, previous: TicketSelection, id: string, payload: Record<string, unknown>,
  teamAction?: JsonObject): { draft: TicketSelection; ready: boolean } {
  const state = isJsonObject(payload.state) ? payload.state : {}
  const values = isJsonObject(state.values) ? state.values : {}
  const teamBlock = `team.${previous.team}`
  const team = selected(teamAction ?? actionValue(values, teamBlock, TEAM_ACTION + id))
  if (typeof team !== 'string' || !Object.hasOwn(form.teams, team) || (teamAction && teamAction.block_id !== teamBlock)) {
    return { draft: previous, ready: false }
  }
  const project = selected(actionValue(values, `project.${team}`, 'project'))
  const validProject = project === 'none' || form.teams[team]!.some(p => p.id === project)
  const date = actionValue(values, 'due', 'date').selected_date
  const validDate = date === null || (typeof date === 'string' && /^\d{4}-\d\d-\d\d$/.test(date)
    && Number.isFinite(Date.parse(date)) && new Date(date).toISOString().slice(0, 10) === date && !date.startsWith('0000'))
  const note = actionValue(values, 'context', 'note').value
  const validNote = note === null || (typeof note === 'string' && [...note].length <= 2000)
  const draft = { team, project: validProject ? (project === 'none' ? null : project as string)
    : team === previous.team ? previous.project : team === form.team ? form.project : null,
    due: validDate ? date as string | null : previous.due,
    context: validNote ? (note as string | null) ?? '' : previous.context }
  return { draft, ready: team === previous.team && validProject && validDate && validNote }
}
