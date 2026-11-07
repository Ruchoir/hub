# The hub

- Status: accepted (decisions of section 9 taken by Théo, 2026-11-07)
- Date: 2026-11-07

The hub holds what every product needs and none should own alone ([.github ADR 0003]):

- the inbox;
- web push, and phone push through the publisher's relay;
- the digests of what is unread, by mail;
- live delivery to connected pages and apps;
- presence;
- the delivery of server events between services ([.github ADR 0015]).

This design says what moves from the monolith, what it becomes, and what the products publish.

Decisions it builds on, not reopened here:

- Products compute their audience; **the hub never decides who may see what** (ADR 0003).
- A push carries no content, only a constant marker (`dependency-exceptions.md`).
- Server events are `ruchoir-events`, signed by their emitter, forwarded unchanged (ADR 0011, 0015).
- A page opens its socket with a single-use ticket its product obtains (ADR 0016).

## 1. What exists

In the monolith (`apps/api/src`):

- **Live delivery** (`realtime/`, about 1,200 lines):
  - one WebSocket (or SSE) per session, authenticated by the session, which receives everything
    addressed to its account;
  - the publisher computes the audience and calls `Hub::publish(audience, envelope)`, which
    `PUBLISH`es on one Valkey channel; every process delivers to its own connections;
  - a full send buffer drops the event, and the client resyncs over REST;
  - typing goes from the page to the server over the socket.
- **Presence**: a Valkey key per account, refreshed by the socket, plus a manual choice on `users`
  (`away`, `dnd`, `invisible`). It is broadcast to everyone who shares a space.
- **The inbox** (`messaging/notifications.rs`, `notifications` table):
  - rows hold foreign keys to messages, conversations and calendar events;
  - the feed **hydrates** them by reading those products' tables: the message body, the channel name,
    the calendar event.
- **Push** (`notify/`, about 2,900 lines):
  - web push with VAPID keys kept encrypted in `instance_settings`, and a content-free payload; the
    service worker then fetches `/push/pending` for the words;
  - phones are woken through the relay;
  - a mail digest of what is unread after 15 minutes (messages only; the calendar mails itself);
  - preferences in three layers (conversation, space, account), with quiet hours and do not disturb.

## 2. What the hub holds

| Hub | From | Changes |
|---|---|---|
| `inbox` | `notifications` | No foreign key to a product. An entry carries what its product published (section 3), and refers to the product's object by an opaque id and a link |
| `push_subscriptions`, `push_devices`, the VAPID keys | the same tables, `instance_settings` | Unchanged |
| `notification_settings` | the notification part of `user_preferences`, `space_notification_prefs` | An account's settings and its level per organisation |
| presence | `presence:<id>` keys, `users.manual_presence` | Unchanged; the manual choice moves from Propolis's `users` to the hub |
| `organisation_members` | (none) | A replica of who belongs to which organisation, from Propolis's events, so that presence reaches the right people (section 5) |
| `event_subscriptions`, `event_deliveries` | (none) | Who receives which event kind, and what is still to deliver (section 7) |

What stays with the products:

- a conversation's level and mute (`channel_members`, `dm_participants`) stay with the chat;
- the calendar's reminder settings stay with the calendar;
- read cursors stay with the chat.

## 3. What a product publishes to the inbox

The hub cannot read a product's tables. Today's hydration therefore becomes data the product sends
when it publishes:

- `kind`, namespaced by product (`chat.mention`, `calendar.reminder`);
- `category`, one of a few the hub knows (`mention`, `reply`, `direct`, `activity`, `reminder`,
  `invitation`, `notice`), which decides urgency, preferences and mail;
- `organisation`, `actor`, `object` (an opaque id), `link` (where the entry leads);
- `variables`: the names and the excerpt the entry shows (`{"place": "#général", "excerpt": "..."}`),
  as they stood when it was published;
- `recipients`: the accounts it is for, as the product computed them.

The words are a question (section 9.1).

## 4. Preferences

- **The product applies what only it knows:** a conversation's level and mute, and who receives a
  plain message (`every_message_recipients` today). Its `recipients` are already filtered.
- **The hub applies the rest:**
  - the account's settings by category and channel (inbox, push, mail);
  - the level per organisation;
  - quiet hours;
  - do not disturb.

  `may_interrupt` and the mail deferral move unchanged.

## 5. Presence

- Unchanged in Valkey: a heartbeat key per account, refreshed by its sockets, plus the manual choice.
- Who sees a person's presence: whoever shares an organisation with them. The hub keeps the members of
  each organisation from Propolis's `member.*` and `organisation.*` events. A member who missed events
  is rebuilt from Propolis's administration API.
- One fix along the way: today a process clears the key when *its* last connection closes, even if
  another process still holds one. The hub counts connections in Valkey instead.

## 6. Live delivery

- **Publishing.** A product sends `Publication` (`ruchoir-events`): an audience of account ids and a
  `ClientFrame`. The hub fans it out as today, through one Valkey channel.
- **Connecting.**
  - A page uses a single-use ticket, a minute long, obtained by its product (ADR 0016).
  - An app uses its access token for the hub.
  - The socket only receives, apart from its pings. Typing goes from the page to its product, which
    publishes it.
- **Ending.**
  - Propolis's back-channel logout, and its `account.signed_out_everywhere` event, close the sockets
    of the sessions concerned, across processes. Today `close_session` only reaches the local process.
  - Signing out everywhere also forgets the person's phones (`forget_user`).
- **Limits** unchanged: a buffer of 256 frames per connection, a 45-second heartbeat.

## 7. Server events (ADR 0015)

- **Subscriptions.** Each service declares the event kinds it consumes and its delivery endpoint at
  install (`POST /api/v1/events` on its own host). The installer registers them with the hub.
- **Delivery.** The emitter posts the signed bytes to the hub, which answers once they are stored.
  The hub then delivers to each subscriber, retrying with backoff for 24 hours, then gives up and logs
  it.
- **Checks.** The hub checks the signature before storing, and refuses a kind whose prefix is not
  the emitter's (`drive.` from the drive). Consumers check again.

## 8. Who may call the hub

| Caller | Proves itself with |
|---|---|
| A product publishing (inbox, frames, events) | A `client_credentials` token from Propolis, scope `hub:publish` (ADR 0006); events are also signed |
| A product asking for a socket ticket | A token exchanged for the person (RFC 8693) |
| A page or an app on a socket | The ticket, or the app's token for the hub |
| A person reading their inbox and settings | Their token for the hub (through their product's backend, or an app) |

**All of it needs Propolis's OpenID Connect provider (step 2.3).** Without it, no call reaching the
hub can be authenticated.

## 9. Decided

Taken by Théo on 2026-11-07:

1. **The words of an entry come from its category and variables.** The interfaces draw them from
   their dictionaries. For mail and a phone's lock screen, the hub holds a small set of sentences per
   category, in six languages.
2. **Preferences are shared.** The product applies a conversation's level and mute before it
   publishes. The hub applies the account's settings, the level per organisation, quiet hours and do
   not disturb.
3. **Presence reaches whoever shares an organisation**, from the hub's replica of the members, fed by
   Propolis's events.
4. **Propolis's OpenID Connect provider (2.3) comes first**, then the hub on its tokens.

[.github ADR 0003]: https://github.com/Ruchoir/.github/blob/main/docs/adr/0003-hub-notifications-and-real-time.md
[.github ADR 0015]: https://github.com/Ruchoir/.github/blob/main/docs/adr/0015-server-events-travel-through-the-hub.md
