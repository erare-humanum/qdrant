import pathlib

from .utils import *

N_PEERS = 3
N_SHARDS = 4
N_REPLICA = 2


def upsert_points(peer_url, city):
    # Create points in first peer's collection
    r_batch = requests.put(
        f"{peer_url}/collections/test_collection/points?wait=true", json={
            "points": [
                {"id": 1, "vector": [0.05, 0.61, 0.76, 0.74], "payload": {"city": city}},
                {"id": 2, "vector": [0.19, 0.81, 0.75, 0.11], "payload": {"city": city}},
                {"id": 3, "vector": [0.36, 0.55, 0.47, 0.94], "payload": {"city": city}},
                {"id": 4, "vector": [0.18, 0.01, 0.85, 0.80], "payload": {"city": city}},
                {"id": 5, "vector": [0.24, 0.18, 0.22, 0.44], "payload": {"city": city}},
                {"id": 6, "vector": [0.35, 0.08, 0.11, 0.44], "payload": {"city": city}},
                {"id": 7, "vector": [0.45, 0.07, 0.21, 0.04], "payload": {"city": city}},
                {"id": 8, "vector": [0.75, 0.18, 0.91, 0.48], "payload": {"city": city}},
                {"id": 9, "vector": [0.30, 0.01, 0.10, 0.12], "payload": {"city": city}},
                {"id": 10, "vector": [0.95, 0.8, 0.17, 0.19], "payload": {"city": city}},
            ]
        })
    assert_http_ok(r_batch)


def create_collection(peer_url, collection="test_collection"):
    # Create collection in first peer
    r_batch = requests.put(
        f"{peer_url}/collections/{collection}", json={
            "vectors": {
                "size": 4,
                "distance": "Dot"
            },
            "shard_number": N_SHARDS,
            "replication_factor": N_REPLICA,
        })
    assert_http_ok(r_batch)


def search(peer_url, city):
    q = {
        "vector": [0.2, 0.1, 0.9, 0.7],
        "top": 10,
        "with_vector": False,
        "with_payload": True,
        "filter": {
            "must": [
                {
                    "key": "city",
                    "match": {"value": city}
                }
            ]
        }
    }

    result = requests.post(f"{peer_url}/collections/test_collection/points/search", json=q).json()
    assert "result" in result, result
    assert result["result"] is not None, result
    return result["result"]


def test_remove_node(tmp_path: pathlib.Path):
    assert_project_root()

    peer_api_uris, peer_dirs, bootstrap_uri = start_cluster(tmp_path, N_PEERS)

    create_collection(peer_api_uris[0])
    wait_collection_exists_and_active_on_all_peers(collection_name="test_collection", peer_api_uris=peer_api_uris)

    upsert_points(peer_api_uris[0], "Paris")

    search_result = search(peer_api_uris[0], "Paris")
    assert len(search_result) > 0

    # Kill last peer

    p = processes.pop()
    p.kill()

    # Validate search works with the dead node
    search_result = search(peer_api_uris[0], "Paris")
    assert len(search_result) > 0

    # Validate upsert works with the dead node
    upsert_points(peer_api_uris[0], "Berlin")

    # Assert that there are dead replicas
    wait_for_some_replicas_not_active(peer_api_uris[0], "test_collection")

    # Assert all records were changed
    search_result = search(peer_api_uris[0], "Paris")
    assert len(search_result) == 0

    collection_cluster_info = get_collection_cluster_info(peer_api_uris[0], "test_collection")
    dead_peer = [shard["peer_id"] for shard in collection_cluster_info["remote_shards"] if shard['state'] != 'Active']

    assert len(dead_peer) > 1

    # force remove dead peer
    res = requests.delete(f"{peer_api_uris[0]}/cluster/peer/{dead_peer[0]}?force=true")
    assert_http_ok(res)

    # Assert all records were changed
    search_result = search(peer_api_uris[0], "Paris")
    assert len(search_result) == 0

    # Assert all records were changed
    search_result = search(peer_api_uris[0], "Berlin")
    assert len(search_result) > 0

