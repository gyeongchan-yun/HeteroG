import numpy as np
import time
import tensorflow as tf

from data import get_all_data
from model import Model
from environment import evaluate_logp
from utils import save, load

import sys
def info(*args):
    print(*args, file=sys.stdout, flush=True)

try:
    records = load("records")
    info("load saved records")
except:
    records = get_all_data()
    info("no saved records")

with tf.device("/gpu:0"):
    model = Model(4, 1, 2, 2)

    try:
        model.load_weights('weights')
        info("load saved weight")
    except:
        info("no saved weight")

    optimizer = tf.keras.optimizers.SGD(learning_rate=.000001, clipnorm=6.)

    for epoch in range(10000):
        record = records[np.random.randint(len(records))]

        cnfeats = tf.convert_to_tensor(record["cnfeats"], dtype=tf.float32)
        cefeats = tf.convert_to_tensor(record["cefeats"], dtype=tf.float32)
        tnfeats = tf.convert_to_tensor(record["tnfeats"], dtype=tf.float32)
        tefeats = tf.convert_to_tensor(record["tefeats"], dtype=tf.float32)
        model.set_graphs(record["cgraph"], record["tgraph"])

        with tf.GradientTape() as tape:
            tape.watch(model.trainable_weights)
            logp = model([cnfeats, cefeats, tnfeats, tefeats], training=True)
            mask, loss_env = evaluate_logp(record, logp.numpy()) # numpy to turn off gradient tracking
            loss = -tf.reduce_sum(tf.boolean_mask(logp, mask))
            # for weight in model.trainable_weights:
            #     loss = loss + 0.000001 * tf.nn.l2_loss(weight)
            grads = tape.gradient(loss, model.trainable_weights)
            # info([tf.reduce_mean(tf.abs(grad)).numpy() for grad in grads])
            optimizer.apply_gradients(zip(grads, model.trainable_weights))

        if epoch % 10 == 0:
            model.save_weights('weights')
            save(records, "records")

        p = np.argmax(mask, axis=2)
        count = {}
        for i in range(p.shape[0]):
            d = tuple(p[i, :])
            count[d] = count.get(d, 0) + 1
        info(count)
        info("loss_env: ", loss_env)

        p = np.argmax(logp.numpy(), axis=2)
        count = {}
        for i in range(p.shape[0]):
            d = tuple(p[i, :])
            count[d] = count.get(d, 0) + 1
        info(count)
        info("loss: ", loss.numpy() / 100000)
