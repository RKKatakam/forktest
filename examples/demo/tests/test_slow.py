import time

import pytest


@pytest.mark.parametrize("n", range(4))
def test_slow(n):
    time.sleep(0.25)
    assert n >= 0
