# Nesayma Team Agents — Design Spec

**Date:** 2026-03-26
**Status:** Approved by panel (Design Expert, Business Strategist, Product Owner)
**Scope:** Database-backed agent teams with persona editing, workflow definition, and UI management

---

## 1. Problem Statement

ZeroClaw needs a structured team of AI agents (the "Nesayma team") that collaborate on website generation projects. The system must:

1. Support 7 predefined agents with rich personas, seeded on fresh install
2. Allow customers to create and manage their own teams through the UI
3. Define how agents collaborate via structured workflows (approval chains)
4. Default all agents to the **minimax** provider
5. Generate appropriate swarm/delegate config from team definitions

---

## 2. Data Model

### 2.1 Teams Table

```sql
CREATE TABLE IF NOT EXISTS teams (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT,
    is_default INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
```

- `is_default = 1` for the seeded Nesayma team (protected from deletion)

### 2.2 Team Members Table

```sql
CREATE TABLE IF NOT EXISTS team_members (
    id TEXT PRIMARY KEY,
    team_id TEXT NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    role_title TEXT NOT NULL,
    persona TEXT NOT NULL,
    avatar_color TEXT NOT NULL DEFAULT '#6366f1',
    provider TEXT NOT NULL DEFAULT 'minimax',
    model TEXT NOT NULL DEFAULT '',
    system_prompt TEXT,
    temperature REAL DEFAULT 0.7,
    sort_order INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_team_members_team ON team_members(team_id);
```

- `persona` — the rich personality description (multi-paragraph)
- `system_prompt` — optional override; if empty, generated from persona at runtime
- `avatar_color` — hex color for the member's avatar circle (initials-based)
- `model` — defaults to empty string, meaning "use instance default model"
- `sort_order` — display ordering within the team

### 2.3 Team Workflows Table

```sql
CREATE TABLE IF NOT EXISTS team_workflows (
    id TEXT PRIMARY KEY,
    team_id TEXT NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    description TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_team_workflows_team ON team_workflows(team_id);
```

### 2.4 Workflow Steps Table

```sql
CREATE TABLE IF NOT EXISTS team_workflow_steps (
    id TEXT PRIMARY KEY,
    workflow_id TEXT NOT NULL REFERENCES team_workflows(id) ON DELETE CASCADE,
    step_order INTEGER NOT NULL,
    name TEXT NOT NULL,
    step_type TEXT NOT NULL,
    participant_ids TEXT NOT NULL DEFAULT '[]',
    description TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_workflow_steps_workflow ON team_workflow_steps(workflow_id);
```

- `step_type` — one of: `generate`, `parallel_review`, `consolidate`, `align`, `qa_gate`, `deliver`
- `participant_ids` — JSON array of team_member IDs assigned to this step

---

## 3. Seed Data — Nesayma Team

### 3.1 Team Members

All members use `provider = "minimax"` and `model = ""` (instance default).

| # | Name | Role Title | Avatar Color | Persona |
|---|------|-----------|-------------|---------|
| 1 | Danni | Operations Lead | #6366f1 (indigo) | Danni is the engine of execution. She turns vision into clear action plans, assigns ownership, tracks deadlines, and makes sure every commitment lands on time. She's persistent, detail-focused, and never lets things drift. In the family, she's known for "the follow-up," and in the business, that's exactly why things get done. |
| 2 | Samia | Competitive Intelligence Lead | #ec4899 (pink) | Samia keeps a constant pulse on the market. She studies top competitors, identifies what they're doing well, and flags exactly where they're vulnerable. Her eye for detail is exceptional, and her review bar is famously high. If it passes Samia, it meets the Samia Sharfi Standard: sharp, rigorous, and launch-ready. |
| 3 | Mamoun | Pitch & Vision Lead | #f59e0b (amber) | Mamoun is the voice of the vision and the face of outreach. He leads customer pitches with clarity, conviction, and resilience, building confidence from the first conversation. He keeps the long-term mission front and center, handles objections calmly, and drives momentum in every opportunity pipeline. With Mamoun leading outreach, the vision is both heard and believed. |
| 4 | Walied | Strategy & Growth Lead | #10b981 (emerald) | Walied translates business ambition into scalable strategy. He audits your website and business model, spots growth levers, and builds practical plans to increase revenue and efficiency. From strategic direction to financial clarity, he delivers frameworks and spreadsheets that help you make smarter decisions and strengthen your bottom line. |
| 5 | Ruba | Design Specialist | #f43f5e (rose) | Ruba is the team's creative soul. She transforms ideas into elegant, on-brand experiences that feel both intentional and memorable. With a strong design instinct and user-first mindset, she brings clarity to every visual decision, from concept to final polish. Ruba makes sure every touchpoint looks refined, feels human, and represents your brand at its best. |
| 6 | Moe | Customer Engagement Lead | #3b82f6 (blue) | Moe leads customer engagement with the instincts of a social media expert. He understands how audiences think, what gets attention, and what builds lasting trust. He shapes communication strategies that strengthen relationships across channels, turning followers into active communities and customers into loyal advocates. Moe keeps your brand close to people, not just visible to them. |
| 7 | Rayan | Quality Assurance Lead | #8b5cf6 (violet) | Rayan is the final quality gate before anything goes live. She runs deep QA, regression checks, and polish reviews to ensure your output is flawless. Her standard is simple: no rough edges, no missed details, no compromises. If Rayan signs off, it's ready for customer delivery at the highest level. |

### 3.2 Default Workflow — "Website Project"

| Step | Name | Type | Participants | Description |
|------|------|------|-------------|-------------|
| 1 | Generation Draft | generate | (all) | Initial website output generation |
| 2 | Role Reviews | parallel_review | Samia, Walied, Ruba, Moe | Parallel expert reviews: competitive analysis, strategy, design, engagement |
| 3 | Feedback Consolidation | consolidate | Danni | Compile all feedback into one prioritized action list |
| 4 | Client Alignment | align | Mamoun | Confirm trade-offs with customer vision |
| 5 | QA Gate | qa_gate | Rayan | Full regression, responsiveness, polish, spec compliance |
| 6 | Final Delivery | deliver | Mamoun | Present finished product to customer |

**Approval rule:** No website is "final" unless each reviewing role gives either "Approved" or "Approved with notes resolved."

---

## 4. REST API

### 4.1 Endpoints

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/teams` | List all teams |
| POST | `/api/teams` | Create a new team |
| GET | `/api/teams/:id` | Get team with members and workflow |
| PUT | `/api/teams/:id` | Update team name/description |
| DELETE | `/api/teams/:id` | Delete team (blocked if `is_default`) |
| POST | `/api/teams/:id/members` | Add member to team |
| PUT | `/api/teams/:id/members/:mid` | Update member persona/config |
| DELETE | `/api/teams/:id/members/:mid` | Remove member |
| PUT | `/api/teams/:id/workflow` | Replace workflow steps |
| GET | `/api/teams/:id/config` | Get generated TOML config snippet |

### 4.2 Response Shapes

**GET /api/teams/:id** returns:
```json
{
  "team": {
    "id": "nesayma",
    "name": "Nesayma",
    "description": "...",
    "is_default": true,
    "members": [
      {
        "id": "danni",
        "name": "Danni",
        "role_title": "Operations Lead",
        "persona": "...",
        "avatar_color": "#6366f1",
        "provider": "minimax",
        "model": "",
        "temperature": 0.7,
        "sort_order": 0
      }
    ],
    "workflow": {
      "id": "website-project",
      "name": "Website Project",
      "steps": [
        {
          "id": "step-1",
          "step_order": 1,
          "name": "Generation Draft",
          "step_type": "generate",
          "participant_ids": ["danni","samia","mamoun","walied","ruba","moe","rayan"],
          "description": "Initial website output generation"
        }
      ]
    }
  }
}
```

---

## 5. Web UI

### 5.1 Route & Navigation

- New route: `/team` in the sidebar
- Icon: `Users` from lucide-react
- Label: "Team"
- Position: after "Agent" in sidebar

### 5.2 Page Layout — Three Tabs

#### Tab 1: Members

```
┌─────────────────────────────────────────────────────────────┐
│  Team: Nesayma                                    [+ Add]   │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐      │
│  │  ┌──┐        │  │  ┌──┐        │  │  ┌──┐        │      │
│  │  │DA│ Danni   │  │  │SA│ Samia   │  │  │MA│ Mamoun  │      │
│  │  └──┘        │  │  └──┘        │  │  └──┘        │      │
│  │  Operations  │  │  Competitive │  │  Pitch &     │      │
│  │  Lead        │  │  Intelligence│  │  Vision Lead │      │
│  │              │  │  Lead        │  │              │      │
│  │  "She turns  │  │  "She keeps  │  │  "He leads   │      │
│  │   vision..." │  │   a pulse.." │  │   pitches.." │      │
│  │              │  │              │  │              │      │
│  │  [Edit]      │  │  [Edit]      │  │  [Edit]      │      │
│  └──────────────┘  └──────────────┘  └──────────────┘      │
│                                                             │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐      │
│  │  ┌──┐        │  │  ┌──┐        │  │  ┌──┐        │      │
│  │  │WA│ Walied  │  │  │RU│ Ruba   │  │  │MO│ Moe    │      │
│  │  └──┘        │  │  └──┘        │  │  └──┘        │      │
│  │  Strategy &  │  │  Design      │  │  Customer    │      │
│  │  Growth Lead │  │  Specialist  │  │  Engagement  │      │
│  │  [Edit]      │  │  [Edit]      │  │  [Edit]      │      │
│  └──────────────┘  └──────────────┘  └──────────────┘      │
│                                                             │
│  ┌──────────────┐                                           │
│  │  ┌──┐        │                                           │
│  │  │RA│ Rayan  │                                           │
│  │  └──┘        │                                           │
│  │  QA Lead     │                                           │
│  │  [Edit]      │                                           │
│  └──────────────┘                                           │
└─────────────────────────────────────────────────────────────┘
```

**Edit Panel (slide-out from right):**
```
┌──────────────────────────────┐
│  Edit Member                 │
│  ─────────────────────────── │
│  Name:     [Danni          ] │
│  Role:     [Operations Lead] │
│                              │
│  Persona:                    │
│  ┌──────────────────────────┐│
│  │ Danni is the engine of   ││
│  │ execution. She turns     ││
│  │ vision into clear action ││
│  │ plans, assigns ownership,││
│  │ tracks deadlines...      ││
│  └──────────────────────────┘│
│                              │
│  Avatar Color:               │
│  [●] [●] [●] [●] [●] [●]   │
│                              │
│  Provider:  [minimax    ▼]   │
│  Model:     [          ]     │
│  Temp:      [0.7       ]     │
│                              │
│  [Cancel]          [Save]    │
└──────────────────────────────┘
```

#### Tab 2: Workflow

```
┌─────────────────────────────────────────────────────────────┐
│  Workflow: Website Project                        [Edit]    │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  ① Generation Draft                                         │
│  ┌─────────────────────────────────────────────────────┐    │
│  │  Type: generate   Participants: All Members          │    │
│  │  Initial website output generation                   │    │
│  └──────────────────────┬──────────────────────────────┘    │
│                         │                                    │
│                         ▼                                    │
│  ② Role Reviews (Parallel)                                  │
│  ┌────────┐ ┌────────┐ ┌────────┐ ┌────────┐              │
│  │ SA     │ │ WA     │ │ RU     │ │ MO     │              │
│  │ Samia  │ │ Walied │ │ Ruba   │ │ Moe    │              │
│  │Competi-│ │Strategy│ │Design  │ │Engage- │              │
│  │tive    │ │& Conv. │ │& UX    │ │ment    │              │
│  └───┬────┘ └───┬────┘ └───┬────┘ └───┬────┘              │
│      └──────────┴──────────┴──────────┘                     │
│                         │                                    │
│                         ▼                                    │
│  ③ Feedback Consolidation                                   │
│  ┌─────────────────────────────────────────────────────┐    │
│  │  DA  Danni — Compiles all feedback into one list     │    │
│  └──────────────────────┬──────────────────────────────┘    │
│                         │                                    │
│                         ▼                                    │
│  ④ Client Alignment                                         │
│  ┌─────────────────────────────────────────────────────┐    │
│  │  MA  Mamoun — Confirms trade-offs with customer      │    │
│  └──────────────────────┬──────────────────────────────┘    │
│                         │                                    │
│                         ▼                                    │
│  ⑤ QA Gate                                                  │
│  ┌─────────────────────────────────────────────────────┐    │
│  │  RA  Rayan — Full regression & polish review         │    │
│  └──────────────────────┬──────────────────────────────┘    │
│                         │                                    │
│                         ▼                                    │
│  ⑥ Final Delivery                                           │
│  ┌─────────────────────────────────────────────────────┐    │
│  │  MA  Mamoun — Presents to customer                   │    │
│  └─────────────────────────────────────────────────────┘    │
│                                                             │
│  Rule: No website is "final" unless each reviewing role     │
│  gives "Approved" or "Approved with notes resolved."        │
└─────────────────────────────────────────────────────────────┘
```

#### Tab 3: Settings

```
┌─────────────────────────────────────────────────────────────┐
│  Team Settings                                              │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  Team Name:    [Nesayma                               ]     │
│  Description:  [Expert panel for website projects     ]     │
│                                                             │
│  Default Provider: [minimax ▼]                              │
│  Default Model:    [                                  ]     │
│                                                             │
│  ┌─────────────────────────────────────────────────────┐    │
│  │  Generated Config (read-only)                        │    │
│  │  ──────────────────────────────────────────────────  │    │
│  │  [agents.danni]                                      │    │
│  │  provider = "minimax"                                │    │
│  │  system_prompt = "You are Danni..."                  │    │
│  │  ...                                                 │    │
│  │                                                      │    │
│  │  [swarms.nesayma]                                    │    │
│  │  agents = ["danni","samia","mamoun",...]              │    │
│  │  strategy = "sequential"                             │    │
│  └─────────────────────────────────────────────────────┘    │
│                                                             │
│  [Delete Team]  (disabled for default team)    [Save]       │
└─────────────────────────────────────────────────────────────┘
```

---

## 6. Implementation Scope

### 6.1 Backend (Rust)

1. **New module: `src/teams/`**
   - `mod.rs` — module declaration
   - `store.rs` — SQLite CRUD for teams, members, workflows, steps
   - `seed.rs` — Nesayma team seed data
   - `api.rs` — Axum handlers for REST endpoints
   - `types.rs` — Request/response structs

2. **Migration** — Add tables in `src/memory/sqlite.rs` `init_schema()` following existing pattern

3. **Gateway integration** — Register team routes in `src/gateway/api.rs`

4. **Config generation** — Generate `[agents.*]` and `[swarms.*]` TOML from team data

### 6.2 Frontend (React)

1. **New page: `web/src/pages/Team.tsx`** — Main team management page with 3 tabs
2. **New types: `web/src/types/api.ts`** — Team, TeamMember, TeamWorkflow, WorkflowStep interfaces
3. **New API functions: `web/src/lib/api.ts`** — CRUD functions for teams
4. **Routing: `web/src/App.tsx`** — Add `/team` route
5. **Navigation: `web/src/components/layout/Sidebar.tsx`** — Add Team nav item

### 6.3 Seed Migration

- Runs on `init_schema()` after table creation
- Checks if Nesayma team exists before inserting
- Seeds all 7 members with full personas
- Seeds the Website Project workflow with 6 steps
- All agents default to `provider = "minimax"`, `model = ""`

---

## 7. Out of Scope (V1)

- Live workflow orchestration (execution tracking, step status)
- Multi-workflow support per team
- Team templates / marketplace
- Agent-to-agent chat visualization
- Drag-and-drop workflow editor (edit via modal in V1)

---

## 8. Acceptance Criteria

1. Fresh install seeds the Nesayma team with 7 members and website workflow
2. `/team` page displays member cards with initials avatars and personas
3. Users can edit any member's name, role, persona, provider, model, temperature
4. Workflow tab shows the 6-step pipeline visually
5. Users can create new teams with custom members and workflows
6. All agents default to minimax provider
7. Settings tab shows generated TOML config
8. Default team cannot be deleted
9. API endpoints return proper JSON for all CRUD operations
10. Existing functionality (agents, swarms, delegate tool) is unaffected
