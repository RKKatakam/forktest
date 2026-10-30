import jax
import jax.numpy as jnp


def dense(w, x):
    return jnp.tanh(w @ x)


@jax.jit
def mlp(w1, w2, x):
    return dense(w2, dense(w1, x))


def loss(w1, w2, x, y):
    return jnp.mean((mlp(w1, w2, x) - y) ** 2)
