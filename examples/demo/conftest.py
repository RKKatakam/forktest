import json
import time

import pytest


@pytest.fixture(scope="session")
def ledger(tmp_path_factory):
    time.sleep(0.02)
    path = tmp_path_factory.mktemp("ledger") / "ledger.json"
    path.write_text(json.dumps({"opening": "100"}))
    return path
