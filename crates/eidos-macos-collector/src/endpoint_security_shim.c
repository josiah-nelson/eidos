#include <EndpointSecurity/EndpointSecurity.h>
#include <stdint.h>
#include <stdlib.h>

typedef void (*eidos_es_callback)(void *context, uint32_t event_kind);

struct eidos_es_lane {
    es_client_t *client;
};

enum {
    EIDOS_ES_SUCCESS = 0,
    EIDOS_ES_NOT_ENTITLED = 1,
    EIDOS_ES_NOT_PERMITTED = 2,
    EIDOS_ES_NOT_PRIVILEGED = 3,
    EIDOS_ES_TOO_MANY_CLIENTS = 4,
    EIDOS_ES_INTERNAL = 5
};

static int eidos_map_result(es_new_client_result_t result) {
    switch (result) {
        case ES_NEW_CLIENT_RESULT_SUCCESS: return EIDOS_ES_SUCCESS;
        case ES_NEW_CLIENT_RESULT_ERR_NOT_ENTITLED: return EIDOS_ES_NOT_ENTITLED;
        case ES_NEW_CLIENT_RESULT_ERR_NOT_PERMITTED: return EIDOS_ES_NOT_PERMITTED;
        case ES_NEW_CLIENT_RESULT_ERR_NOT_PRIVILEGED: return EIDOS_ES_NOT_PRIVILEGED;
        case ES_NEW_CLIENT_RESULT_ERR_TOO_MANY_CLIENTS: return EIDOS_ES_TOO_MANY_CLIENTS;
        default: return EIDOS_ES_INTERNAL;
    }
}

int eidos_es_create(struct eidos_es_lane **output,
                    eidos_es_callback callback,
                    void *context) {
    if (output == NULL || callback == NULL) return EIDOS_ES_INTERNAL;
    struct eidos_es_lane *lane = calloc(1, sizeof(*lane));
    if (lane == NULL) return EIDOS_ES_INTERNAL;

    es_new_client_result_t result = es_new_client(&lane->client,
        ^(es_client_t *client, const es_message_t *message) {
            (void)client;
            switch (message->event_type) {
                case ES_EVENT_TYPE_NOTIFY_OPEN: callback(context, 1); break;
                case ES_EVENT_TYPE_NOTIFY_CLOSE: callback(context, 2); break;
                case ES_EVENT_TYPE_NOTIFY_MMAP: callback(context, 3); break;
                case ES_EVENT_TYPE_NOTIFY_EXEC: callback(context, 4); break;
                default: break;
            }
        });
    int mapped = eidos_map_result(result);
    if (mapped != EIDOS_ES_SUCCESS) {
        free(lane);
        return mapped;
    }

    const es_event_type_t events[] = {
        ES_EVENT_TYPE_NOTIFY_OPEN,
        ES_EVENT_TYPE_NOTIFY_CLOSE,
        ES_EVENT_TYPE_NOTIFY_MMAP,
        ES_EVENT_TYPE_NOTIFY_EXEC
    };
    if (es_subscribe(lane->client, events, sizeof(events) / sizeof(events[0]))
        != ES_RETURN_SUCCESS) {
        es_delete_client(lane->client);
        free(lane);
        return EIDOS_ES_INTERNAL;
    }
    *output = lane;
    return EIDOS_ES_SUCCESS;
}

void eidos_es_destroy(struct eidos_es_lane *lane) {
    if (lane == NULL) return;
    es_unsubscribe_all(lane->client);
    es_delete_client(lane->client);
    free(lane);
}
