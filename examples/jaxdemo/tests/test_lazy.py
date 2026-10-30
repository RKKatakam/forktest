"""Touches JAX only inside tests, so collection leaves the backend cold."""
import jax.numpy as jnp

from model.layers import dense


def test_dense_zero_input():
    assert float(jnp.sum(dense(jnp.eye(3), jnp.zeros(3)))) == 0.0


def test_vmap():
    import jax

    out = jax.vmap(lambda v: v * 2)(jnp.arange(6.0).reshape(3, 2))
    assert out.shape == (3, 2)
