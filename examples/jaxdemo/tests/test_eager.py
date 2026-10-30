"""Builds JAX arrays at import, so the backend and its thread pools start during collection."""
import jax
import jax.numpy as jnp
import pytest

from model.layers import dense, loss, mlp

W1 = jnp.eye(4)
W2 = jnp.ones((4, 4)) / 4
X = jnp.arange(4.0)


def test_dense_shape():
    assert dense(W1, X).shape == (4,)


def test_mlp_is_finite():
    assert bool(jnp.all(jnp.isfinite(mlp(W1, W2, X))))


@pytest.mark.parametrize("scale", [0.5, 1.0, 2.0])
def test_grad_runs(scale):
    g = jax.grad(loss)(W1 * scale, W2, X, jnp.zeros(4))
    assert g.shape == (4, 4)


def test_jit_matches_eager():
    assert jnp.allclose(mlp(W1, W2, X), dense(W2, dense(W1, X)))
